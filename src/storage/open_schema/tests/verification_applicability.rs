use super::*;
use crate::storage::{
    StoreOpenRefusalKind, VERIFICATION_APPLICABILITY_INDEX, store_open_refusal_kind,
};

#[test]
fn git_tree_barrier_missing_refuses_without_mutation_and_narrow_step_restores_it() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("barrier.db");
    let store = SqliteStore::open(&database).unwrap();
    assert!(
        store
            .verification_git_tree_applicability_available()
            .unwrap()
    );
    store
        .connection
        .execute_batch(&format!("DROP INDEX {VERIFICATION_APPLICABILITY_INDEX}"))
        .unwrap();
    drop(store);
    let before = std::fs::read(&database).unwrap();
    for mode in [false, true] {
        let result = if mode {
            SqliteStore::open_existing_read_only(&database)
        } else {
            SqliteStore::open(&database)
        };
        let Err(error) = result else {
            panic!("missing barrier admitted");
        };
        assert_eq!(
            store_open_refusal_kind(&error),
            StoreOpenRefusalKind::ProjectionRepairRequired
        );
        assert_eq!(std::fs::read(&database).unwrap(), before);
    }
    assert_eq!(
        SqliteStore::build_verification_applicability_index(&database).unwrap(),
        0
    );
    let store = SqliteStore::open(&database).unwrap();
    assert!(
        store
            .verification_git_tree_applicability_available()
            .unwrap()
    );
    drop(store);
    assert_eq!(
        SqliteStore::build_verification_applicability_index(&database).unwrap(),
        0
    );
}

#[test]
fn git_tree_barrier_checks_definition_and_census_rolls_creation_back() {
    for private_value in [serde_json::json!("private-value"), serde_json::Value::Null] {
        let directory = crate::test_support::temp_home().unwrap();
        let database = directory.path().join("barrier-census.db");
        let store = SqliteStore::open(&database).unwrap();
        let bytes =
            serde_json::to_vec(&serde_json::json!({"unsupported_member":private_value})).unwrap();
        // Malformed id and orphaned history still require a named strict decode.
        store.connection.execute("INSERT INTO objects(object_id,object_kind,canonical_json) VALUES (?1,'verification_evidence',?2)", params!["malformed-orphan", bytes]).unwrap();
        let report = store.verify_all().unwrap();
        assert!(
            report
                .invalid_control_records
                .iter()
                .any(|finding| finding.contains("unsupported_member"))
        );
        assert!(!format!("{report:?}").contains("private-value"));
        store
            .connection
            .execute_batch(&format!("DROP INDEX {VERIFICATION_APPLICABILITY_INDEX}"))
            .unwrap();
        drop(store);
        let error = SqliteStore::build_verification_applicability_index(&database)
            .unwrap_err()
            .to_string();
        assert!(error.contains("unsupported_member"), "{error}");
        assert!(!error.contains("private-value"), "{error}");
        let connection = Connection::open(&database).unwrap();
        let count: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name=?1",
                [VERIFICATION_APPLICABILITY_INDEX],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }
    let store = SqliteStore::open_in_memory().unwrap();
    store.connection.execute_batch(&format!("DROP INDEX {VERIFICATION_APPLICABILITY_INDEX}; CREATE INDEX {VERIFICATION_APPLICABILITY_INDEX} ON objects(created_at)")).unwrap();
    assert!(
        !store
            .verification_git_tree_applicability_available()
            .unwrap()
    );
}
