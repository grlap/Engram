use super::*;
use crate::storage::test_database_shape_snapshot;
use crate::storage::test_support::*;
use crate::*;
use chrono::{TimeDelta, TimeZone};
use rusqlite::Connection;

mod surviving;

#[test]
fn ordinary_policy_changes_omit_retired_admission_metadata() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let before = store.control_diagnostics().unwrap();
    let changed = store
        .set_obligation_rule_set(
            &ObligationRuleSet {
                schema_version: OBLIGATION_RULE_SET_SCHEMA_VERSION,
                rules: vec![],
            },
            &actor("policy-admin"),
            "empty-rules",
            Some(&before.active_policy),
            Utc::now(),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    assert!(changed.changed);
    for id in [&before.active_policy, &changed.active_policy] {
        let policy: ControlPolicy = store.get(id).unwrap().unwrap();
        assert!(policy.required_assurance.is_none());
        assert!(policy.supported_effects.is_none());
        assert!(policy.grant_ttl_seconds.is_none());
        let bytes: serde_json::Value = serde_json::to_value(&policy).unwrap();
        assert!(bytes.get("required_assurance").is_none());
        assert!(bytes.get("supported_effects").is_none());
        assert!(bytes.get("grant_ttl_seconds").is_none());
    }
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn a_partial_historical_policy_envelope_is_not_read_as_current_policy() {
    let store = SqliteStore::open_in_memory().unwrap();
    let id = store.control_diagnostics().unwrap().active_policy;
    let mut policy: ControlPolicy = store.get(&id).unwrap().unwrap();
    policy.grant_ttl_seconds = Some(30);
    assert!(matches!(
        SqliteStore::validate_control_policy_shape(&policy),
        Err(StoreError::InvalidControlProjection(_))
    ));
}

#[test]
fn policy_actor_refusal_and_receipt_failure_leave_the_whole_store_unchanged() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let rules = ObligationRuleSet {
        schema_version: OBLIGATION_RULE_SET_SCHEMA_VERSION,
        rules: vec![],
    };
    let mut invalid_actor = actor("policy-admin");
    invalid_actor.session_id = Some(SessionId("a".repeat(65)));
    let before = test_database_shape_snapshot(&store.connection).unwrap();
    assert!(
        store
            .set_obligation_rule_set(
                &rules,
                &invalid_actor,
                "invalid-actor",
                None,
                Utc::now(),
                &DevelopmentNoopRedactor
            )
            .is_err()
    );
    assert_eq!(
        test_database_shape_snapshot(&store.connection).unwrap(),
        before
    );
    store.connection.execute_batch("CREATE TEMP TRIGGER fail_policy_receipt BEFORE INSERT ON control_policy_operation_results BEGIN SELECT RAISE(ABORT, 'injected receipt refusal'); END").unwrap();
    let before = test_database_shape_snapshot(&store.connection).unwrap();
    let result = store.set_obligation_rule_set(
        &rules,
        &actor("policy-admin"),
        "receipt-refusal",
        None,
        Utc::now(),
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(result, Err(StoreError::Sqlite(_))));
    assert_eq!(
        test_database_shape_snapshot(&store.connection).unwrap(),
        before
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn obligation_rule_set_activation_is_canonical_idempotent_and_restart_safe() {
    let directory = crate::test_support::temp_home().expect("temporary store directory");
    let database = directory.path().join("obligation-rules.db");
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open(&database).expect("initialize store");
    let initial = store.control_diagnostics().expect("initial diagnostics");
    let stock: ObligationRuleSet = store
        .get(&initial.obligation_rule_set)
        .expect("read stock rule set")
        .expect("stock rule set object");
    assert_eq!(stock, crate::control::builtin_obligation_rule_set());

    let empty = ObligationRuleSet {
        schema_version: OBLIGATION_RULE_SET_SCHEMA_VERSION,
        rules: Vec::new(),
    };
    let changed = store
        .set_obligation_rule_set(
            &empty,
            &actor("rule-policy-admin"),
            "rule-set-empty",
            Some(&initial.active_policy),
            now,
            &DevelopmentNoopRedactor,
        )
        .expect("activate empty rule set");
    assert!(changed.changed);
    assert_eq!(changed.previous_rule_set, Some(initial.obligation_rule_set));
    assert_eq!(changed.policy_epoch, ProjectPolicyEpoch(2));
    let active = store.control_diagnostics().expect("changed diagnostics");
    assert_eq!(active.obligation_rule_set, changed.obligation_rule_set);

    let replay = store
        .set_obligation_rule_set(
            &empty,
            &actor("rule-policy-admin"),
            "rule-set-empty-noop",
            Some(&changed.active_policy),
            now + TimeDelta::seconds(1),
            &DevelopmentNoopRedactor,
        )
        .expect("reapply selected rule set");
    assert!(!replay.changed);
    assert_eq!(replay.active_policy, changed.active_policy);
    drop(store);
    let mut store = SqliteStore::open(&database).expect("reopen rule-set no-op receipt");
    let replay_after_restart = store
        .set_obligation_rule_set(
            &empty,
            &actor("rule-policy-admin"),
            "rule-set-empty-noop",
            Some(&changed.active_policy),
            now + TimeDelta::seconds(10),
            &DevelopmentNoopRedactor,
        )
        .expect("replay selected rule-set no-op after restart");
    assert_eq!(replay_after_restart, replay);
    assert!(matches!(
        store.set_obligation_rule_set(
            &ObligationRuleSet {
                schema_version: OBLIGATION_RULE_SET_SCHEMA_VERSION + 1,
                rules: Vec::new(),
            },
            &actor("rule-policy-admin"),
            "rule-set-invalid-schema",
            None,
            now + TimeDelta::seconds(2),
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::InvalidControlProjection(_))
    ));
    assert!(
        store
            .verify_all()
            .expect("verify rule-set history")
            .is_healthy()
    );
    drop(store);

    let reopened = SqliteStore::open(&database).expect("reopen selected rule set");
    let diagnostics = reopened
        .control_diagnostics()
        .expect("reopened diagnostics");
    assert_eq!(diagnostics.active_policy, changed.active_policy);
    assert_eq!(diagnostics.obligation_rule_set, changed.obligation_rule_set);
    let selected: ObligationRuleSet = reopened
        .get(&changed.obligation_rule_set)
        .expect("read selected rule set")
        .expect("selected rule set object");
    assert_eq!(selected, empty);
}

#[test]
fn returning_to_an_earlier_rule_set_names_the_same_record() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open_in_memory().expect("store");
    let initial = store.control_diagnostics().expect("initial diagnostics");
    let empty = ObligationRuleSet {
        schema_version: OBLIGATION_RULE_SET_SCHEMA_VERSION,
        rules: Vec::new(),
    };
    let emptied = store
        .set_obligation_rule_set(
            &empty,
            &actor("rule-policy-admin"),
            "rule-set-empty",
            Some(&initial.active_policy),
            now,
            &DevelopmentNoopRedactor,
        )
        .expect("activate empty rule set");
    assert_ne!(emptied.obligation_rule_set, initial.obligation_rule_set);
    let records = |store: &SqliteStore| -> i64 {
        store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM objects WHERE object_kind = 'obligation_rule_set'",
                [],
                |row| row.get(0),
            )
            .expect("rule-set records")
    };
    assert_eq!(records(&store), 2);

    let restored = store
        .set_obligation_rule_set(
            &crate::control::builtin_obligation_rule_set(),
            &actor("rule-policy-admin"),
            "rule-set-stock-again",
            Some(&emptied.active_policy),
            now + TimeDelta::seconds(1),
            &DevelopmentNoopRedactor,
        )
        .expect("return to the stock rule set");
    assert!(restored.changed);
    assert_eq!(restored.policy_epoch, ProjectPolicyEpoch(3));
    assert_eq!(
        restored.previous_rule_set,
        Some(emptied.obligation_rule_set)
    );
    // Equal content an earlier policy named is the same record, not a copy.
    assert_eq!(restored.obligation_rule_set, initial.obligation_rule_set);
    assert_eq!(records(&store), 2);
    assert!(store.verify_all().expect("doctor").is_healthy());
}

/// A stored rule set whose requirement names an environment, with a value
/// or as null, is refused where it is read, naming the member; it is never
/// read as the same rule without its pin.
#[test]
fn a_stored_rule_set_that_names_an_environment_is_refused_by_name() {
    let store = SqliteStore::open_in_memory().expect("store");
    let hash = store
        .control_diagnostics()
        .expect("initial diagnostics")
        .obligation_rule_set;
    SqliteStore::load_obligation_rule_set_on(&store.connection, &hash)
        .expect("the stock rule set loads");
    let stored: Vec<u8> = store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_id = ?1",
            [hash.as_str()],
            |row| row.get(0),
        )
        .expect("stock rule set bytes");
    for environment in [
        serde_json::json!(ObjectId::from_canonical_bytes(b"environment")),
        serde_json::Value::Null,
    ] {
        let mut rule_set: serde_json::Value =
            serde_json::from_slice(&stored).expect("rule-set json");
        rule_set["rules"][0]["requirement"]["required_environment"] = environment;
        store
            .connection
            .execute(
                "UPDATE objects SET canonical_json = ?1 WHERE object_id = ?2",
                rusqlite::params![
                    serde_json::to_vec(&rule_set).expect("rule-set bytes"),
                    hash.as_str()
                ],
            )
            .expect("store a rule set that names an environment");
        let error = SqliteStore::load_obligation_rule_set_on(&store.connection, &hash)
            .expect_err("a rule set naming an environment is refused")
            .to_string();
        assert!(
            error.contains("unknown field `required_environment`"),
            "{error}"
        );
        // The doctor names the rule set and the member once, beside the
        // policy records that select it, and counts the rule set it checked.
        let report = store.verify_all().expect("doctor");
        let rule_set_label =
            format!("obligation_rule_set:{hash}:unknown field `required_environment`");
        assert_eq!(
            report
                .invalid_control_records
                .iter()
                .filter(|label| label.starts_with(&rule_set_label))
                .count(),
            1,
            "{report:?}"
        );
        assert!(
            report.invalid_control_records.len() <= report.checked_control_records,
            "{report:?}"
        );
    }
}

#[test]
fn established_store_missing_policy_state_refuses_without_bootstrap() {
    let directory = crate::test_support::temp_home().expect("temporary store directory");

    let missing_row_database = directory.path().join("missing-policy-row.db");
    let missing_row =
        SqliteStore::open(&missing_row_database).expect("initialize missing-row fixture");
    let missing_row_objects = missing_row
        .connection
        .query_row("SELECT COUNT(*) FROM objects", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count missing-row objects");
    missing_row
        .connection
        .execute("DELETE FROM control_policy_state", [])
        .expect("remove policy singleton");
    drop(missing_row);
    assert!(matches!(
        SqliteStore::open(&missing_row_database),
        Err(StoreError::InvalidControlProjection(reason))
            if reason.contains("singleton")
    ));
    let missing_row_raw =
        Connection::open(&missing_row_database).expect("inspect missing-row refusal");
    assert_eq!(
        missing_row_raw
            .query_row("SELECT COUNT(*) FROM control_policy_state", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("singleton remains absent"),
        0
    );
    assert_eq!(
        missing_row_raw
            .query_row("SELECT COUNT(*) FROM objects", [], |row| row
                .get::<_, i64>(0))
            .expect("objects remain unchanged"),
        missing_row_objects
    );

    let missing_table_database = directory.path().join("missing-policy-table.db");
    let missing_table =
        SqliteStore::open(&missing_table_database).expect("initialize missing-table fixture");
    let missing_table_objects = missing_table
        .connection
        .query_row("SELECT COUNT(*) FROM objects", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count missing-table objects");
    missing_table
        .connection
        .execute("DROP TABLE control_policy_state", [])
        .expect("remove policy state table");
    drop(missing_table);
    assert!(matches!(
        SqliteStore::open(&missing_table_database),
        Err(StoreError::DifferentBuildSchema)
    ));
    let missing_table_raw =
        Connection::open(&missing_table_database).expect("inspect missing-table refusal");
    assert!(
        !missing_table_raw
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM sqlite_master
                     WHERE type = 'table' AND name = 'control_policy_state'
                 )",
                [],
                |row| row.get::<_, bool>(0),
            )
            .expect("policy state table remains absent")
    );
    assert_eq!(
        missing_table_raw
            .query_row("SELECT COUNT(*) FROM objects", [], |row| row
                .get::<_, i64>(0))
            .expect("objects remain unchanged"),
        missing_table_objects
    );
}

#[test]
fn current_policy_with_null_selector_returns_a_typed_projection_error() {
    let store = SqliteStore::open_in_memory().expect("store");
    store
        .connection
        .execute(
            "UPDATE control_policy_state SET policy_id = NULL WHERE singleton = 1",
            [],
        )
        .expect("clear active selector");

    assert!(matches!(
        SqliteStore::load_control_policy_head(&store.connection),
        Err(StoreError::InvalidControlProjection(reason))
            if reason.contains("no selected version")
    ));
}

#[test]
fn partial_control_table_family_prevents_policy_rebootstrap() {
    let directory = crate::test_support::temp_home().expect("temporary store directory");
    let database = directory.path().join("ordinary-data.db");
    let mut store = SqliteStore::open(&database).expect("initialize established store");
    let ordinary = store
        .append(
            "example",
            &Example {
                title: "ordinary durable object".into(),
                body: "not control-policy evidence".into(),
            },
        )
        .expect("append ordinary canonical data");
    store
        .connection
        .execute_batch(
            "DROP TABLE control_policy_state;
             DROP TABLE control_policy_versions;
             DELETE FROM objects
             WHERE object_kind IN (
                 'control_policy', 'project_policy_authority_decision',
                 'obligation_rule_set'
             );",
        )
        .expect("remove every control-policy artifact");
    drop(store);

    assert!(matches!(
        SqliteStore::open(&database),
        Err(StoreError::DifferentBuildSchema)
    ));
    let raw = Connection::open(&database).expect("inspect refused established store");
    assert_eq!(
        raw.query_row("SELECT COUNT(*) FROM objects", [], |row| row
            .get::<_, i64>(0))
            .expect("ordinary object remains"),
        1
    );
    assert_eq!(
        raw.query_row(
            "SELECT object_kind FROM objects WHERE object_id = ?1",
            [ordinary.key().as_str()],
            |row| row.get::<_, String>(0),
        )
        .expect("ordinary object identity remains"),
        "example"
    );
    for table in ["control_policy_state", "control_policy_versions"] {
        assert!(
            !raw.query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM sqlite_master
                     WHERE type = 'table' AND name = ?1
                 )",
                [table],
                |row| row.get::<_, bool>(0),
            )
            .expect("policy table remains absent")
        );
    }
}

#[test]
fn control_diagnostics_reuses_an_embedder_owned_snapshot() {
    let store = SqliteStore::open_in_memory().expect("store");
    let snapshot = store
        .connection
        .unchecked_transaction()
        .expect("open caller snapshot");
    let diagnostics = store
        .control_diagnostics()
        .expect("diagnostics reuse caller snapshot");
    assert_eq!(diagnostics.policy_epoch, ProjectPolicyEpoch(1));
    snapshot.rollback().expect("rollback caller snapshot");
}
