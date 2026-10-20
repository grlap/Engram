use super::*;

#[test]
fn folder_native_completion_survives_unrelated_apply_reopen_and_repair() {
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("native-seal.db");
    let mut store = SqliteStore::open(&database).unwrap();
    let fixture = mixed(&mut store);
    let sealed = query::load_work_item(&store.connection, fixture.sealed).unwrap();
    let native = native_rows(&store);
    let old_objects = objects(&store);
    let selected = edited(&exported(&store), fixture.scalar, |task| {
        task.title = "unrelated safe edit".into();
    });
    exchange(&mut store, &selected, "safe-beside-seal", 10).unwrap();
    assert_eq!(
        query::load_work_item(&store.connection, fixture.sealed).unwrap(),
        sealed
    );
    assert_eq!(native_rows(&store), native);
    let after_objects = objects(&store);
    for (id, bytes) in &old_objects {
        assert_eq!(
            after_objects.get(id),
            Some(bytes),
            "old native evidence/object {id}"
        );
    }
    for case in ["outcome", "reopen", "parent", "remove"] {
        let safe = edited(&selected, fixture.scalar, |task| task.priority = 3);
        let changed = match case {
            "outcome" => edited(&safe, fixture.sealed, |task| {
                task.outcome = "foreign result".into();
            }),
            "reopen" => edited(&safe, fixture.sealed, |task| {
                task.lifecycle = WorkLifecycle::Open;
                task.completion = None;
            }),
            "parent" => edited(&safe, fixture.sealed, |task| {
                task.parent_id = Some(fixture.scalar);
            }),
            "remove" => without(&safe, &[fixture.sealed]),
            _ => unreachable!(),
        };
        assert!(changed.validate_folder().is_ok(), "{case}");
        let before = state(&store);
        let message =
            conflict(&exchange(&mut store, &changed, &format!("sealed-{case}"), 11).unwrap_err());
        let reason = match case {
            "outcome" => "restored_completion",
            "remove" => "sealed_completion",
            _ => "unsupported_fields",
        };
        assert!(
            message.contains(&format!("{}: {reason}", fixture.sealed.0)),
            "{message}"
        );
        assert_eq!(state(&store), before, "{case}");
    }
    assert!(store.verify_all().unwrap().is_healthy());
    let durable = all_durable_rows(&store);
    drop(store);
    let store = SqliteStore::open(&database).unwrap();
    assert_eq!(all_durable_rows(&store), durable);
    assert_eq!(native_rows(&store), native);
    drop(store);
    assert!(
        SqliteStore::repair_rebuildable_projections(&database)
            .unwrap()
            .is_healthy()
    );
    let store = SqliteStore::open(&database).unwrap();
    assert_eq!(all_durable_rows(&store), durable);
    assert_eq!(native_rows(&store), native);
    assert_eq!(
        query::load_work_item(&store.connection, fixture.sealed).unwrap(),
        sealed
    );
    assert_eq!(exported(&store), selected);
    assert!(store.verify_all().unwrap().is_healthy());
}
