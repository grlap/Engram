use chrono::{TimeDelta, TimeZone, Utc};
use rusqlite::Connection;

use super::*;
use crate::storage::{store_sidecars, test_database_shape_snapshot, test_support::*};
use crate::*;

use crate::{
    DevelopmentNoopRedactor,
    domain::{ControlAssurance, EffectClass, ProjectId, TurnIntent},
};

#[test]
fn integrity_scanner_covers_enforced_control_records() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open_in_memory().unwrap();
    let binding = bind_control(&mut store, now);
    let decision = store
        .evaluate_control_turn(
            &ProjectId("project-a".into()),
            &SessionId("control-session".into()),
            &binding.connection_token,
            &binding.routing_token,
            &TurnIntent {
                idempotency_key: "integrity-turn-a".into(),
                intent_fingerprint: ObjectHash::from_canonical_bytes(b"integrity-turn-a"),
                purpose: crate::domain::TurnPurpose::Ordinary,
                requested_effects: vec![EffectClass::Observe],
                resource_intents: Vec::new(),
            },
            now + TimeDelta::seconds(1),
        )
        .unwrap();
    let ControlTurnDecision::Grant { grant } = decision else {
        panic!("control integrity fixture must grant");
    };
    let healthy = store.verify_all().unwrap();
    assert!(healthy.is_healthy());
    assert_eq!(healthy.checked_control_records, 5);

    store
        .connection
        .execute(
            "UPDATE control_sessions SET bind_intent_json = ?1",
            params![b"{}".as_slice()],
        )
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE control_turn_results SET decision_json = ?1",
            params![b"{}".as_slice()],
        )
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE control_turn_grants SET grant_json = ?1",
            params![b"{}".as_slice()],
        )
        .unwrap();
    let corrupted = store.verify_all().unwrap();
    assert_eq!(corrupted.checked_control_records, 5);
    assert_eq!(corrupted.invalid_control_records.len(), 3);
    assert!(
        corrupted
            .invalid_control_records
            .contains(&"control_session:control-session".into())
    );
    assert!(
        corrupted
            .invalid_control_records
            .contains(&"control_turn_result:1".into())
    );
    assert!(
        corrupted
            .invalid_control_records
            .contains(&format!("control_turn_grant:{}", grant.grant_id))
    );
}

#[test]
fn diagnostics_only_policy_recovery_names_every_invalid_binding_without_mutation() {
    let directory = tempfile::tempdir().expect("temporary store directory");
    let database = directory.path().join("corrupt-policy.db");
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open(&database).expect("initialize policy fixture");
    let initial = store.control_diagnostics().expect("initial policy");
    let changed = store
        .set_required_control_assurance(
            ControlAssurance::Advisory,
            &actor("recovery-test-admin"),
            "create a second policy version",
            "recovery-test-policy-update",
            Some(&initial.active_policy),
            now,
            &DevelopmentNoopRedactor,
        )
        .expect("activate second policy version");
    store
        .connection
        .execute(
            "UPDATE control_policy_versions SET policy_json = X'7B7D'",
            [],
        )
        .expect("corrupt both policy projections");
    drop(store);

    assert!(matches!(
        SqliteStore::open(&database),
        Err(StoreError::InvalidControlProjection(_))
    ));
    let before = Connection::open(&database).expect("inspect corrupt store");
    let before_shape = test_database_shape_snapshot(&before)
        .expect("capture corrupt store before recovery diagnostics");
    drop(before);
    let before_bytes = std::fs::read(&database).expect("capture corrupt database bytes");

    let report = SqliteStore::diagnose_control_policy_recovery(&database)
        .expect("read-only recovery diagnostics");
    assert!(!report.is_healthy());
    assert_eq!(report.checked_control_records, 3);
    for record in [
        "control_policy_state:active".to_owned(),
        format!("control_policy_version:{}", initial.active_policy),
        format!("control_policy_version:{}", changed.active_policy),
    ] {
        assert!(
            report
                .invalid_control_records
                .iter()
                .any(|finding| finding.record == record),
            "missing recovery finding for {record}"
        );
    }
    assert!(
        report
            .guidance
            .contains("did not select, rewrite, or activate")
    );

    let after = Connection::open(&database).expect("inspect diagnosed store");
    let after_shape = test_database_shape_snapshot(&after)
        .expect("capture corrupt store after recovery diagnostics");
    drop(after);
    let after_bytes = std::fs::read(&database).expect("capture diagnosed database bytes");
    assert_eq!(after_shape, before_shape);
    assert_eq!(after_bytes, before_bytes);
    assert!(matches!(
        SqliteStore::open(&database),
        Err(StoreError::InvalidControlProjection(_))
    ));
}

#[test]
fn diagnostics_only_policy_recovery_reports_missing_and_malformed_columns_without_mutation() {
    let directory = tempfile::tempdir().expect("temporary store directory");
    for (name, policy_epoch_definition, policy_epoch_projection, expected_detail) in [
        (
            "missing-column",
            None,
            None,
            "required column \"policy_epoch\" is missing",
        ),
        (
            "malformed-column",
            Some("policy_epoch TEXT NOT NULL"),
            Some("CAST(policy_epoch AS TEXT)"),
            "declared type \"TEXT\"; expected INTEGER",
        ),
    ] {
        let database = directory.path().join(format!("{name}.db"));
        drop(SqliteStore::open(&database).expect("initialize recovery fixture"));
        let fixture = Connection::open(&database).expect("open recovery fixture");
        fixture
            .execute_batch("PRAGMA foreign_keys = OFF; BEGIN IMMEDIATE;")
            .expect("begin schema corruption fixture");
        fixture
            .execute_batch("ALTER TABLE control_policy_state RENAME TO old_policy_state;")
            .expect("rename current policy state");
        let epoch_column = policy_epoch_definition
            .map(|definition| format!("{definition},"))
            .unwrap_or_default();
        fixture
            .execute_batch(&format!(
                "CREATE TABLE control_policy_state (
                     singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
                     schema_version INTEGER NOT NULL,
                     {epoch_column}
                     required_assurance TEXT NOT NULL,
                     supported_effects_json TEXT NOT NULL,
                     grant_ttl_seconds INTEGER NOT NULL,
                     policy_hash TEXT REFERENCES objects(object_hash)
                 ) STRICT;"
            ))
            .expect("create malformed policy state");
        let (insert_columns, select_columns) = policy_epoch_projection.map_or_else(
            || {
                (
                    "singleton, schema_version, required_assurance, supported_effects_json, grant_ttl_seconds, policy_hash".to_owned(),
                    "singleton, schema_version, required_assurance, supported_effects_json, grant_ttl_seconds, policy_hash".to_owned(),
                )
            },
            |projection| {
                (
                    "singleton, schema_version, policy_epoch, required_assurance, supported_effects_json, grant_ttl_seconds, policy_hash".to_owned(),
                    format!("singleton, schema_version, {projection}, required_assurance, supported_effects_json, grant_ttl_seconds, policy_hash"),
                )
            },
        );
        fixture
            .execute_batch(&format!(
                "INSERT INTO control_policy_state ({insert_columns})
                 SELECT {select_columns} FROM old_policy_state;
                 DROP TABLE old_policy_state;
                 COMMIT;
                 PRAGMA foreign_keys = ON;
                 PRAGMA wal_checkpoint(TRUNCATE);"
            ))
            .expect("finish schema corruption fixture");
        drop(fixture);
        for sidecar in store_sidecars(&database) {
            let _ = std::fs::remove_file(sidecar);
        }

        let before = Connection::open(&database).expect("inspect malformed recovery store");
        let before_shape =
            test_database_shape_snapshot(&before).expect("capture malformed recovery shape");
        drop(before);
        let before_bytes = std::fs::read(&database).expect("capture malformed recovery bytes");

        let report = SqliteStore::diagnose_control_policy_recovery(&database)
            .expect("malformed schema returns typed recovery findings");
        assert!(report.invalid_control_records.iter().any(|finding| {
            finding.record == "control_policy_state:schema"
                && finding.detail.contains(expected_detail)
        }));

        let after = Connection::open(&database).expect("inspect diagnosed recovery store");
        let after_shape =
            test_database_shape_snapshot(&after).expect("capture diagnosed recovery shape");
        drop(after);
        assert_eq!(after_shape, before_shape);
        assert_eq!(
            std::fs::read(&database).expect("read diagnosed recovery bytes"),
            before_bytes
        );
    }
}

#[test]
fn missing_policy_rows_are_reported_as_integrity_records() {
    let version_store = SqliteStore::open_in_memory().expect("version fixture");
    let active = version_store
        .control_diagnostics()
        .expect("version diagnostics")
        .active_policy;
    version_store
        .connection
        .execute(
            "DELETE FROM control_policy_versions WHERE policy_hash = ?1",
            [active.as_str()],
        )
        .expect("delete projected version row");
    let report = version_store
        .verify_all()
        .expect("scan missing version row");
    assert!(
        report
            .invalid_control_records
            .contains(&"control_policy_state:active".into())
    );

    let object_store = SqliteStore::open_in_memory().expect("object fixture");
    let active = object_store
        .control_diagnostics()
        .expect("object diagnostics")
        .active_policy;
    object_store
        .connection
        .execute_batch("PRAGMA foreign_keys = OFF;")
        .expect("disable fixture foreign keys");
    object_store
        .connection
        .execute(
            "DELETE FROM objects WHERE object_hash = ?1",
            [active.as_str()],
        )
        .expect("delete canonical policy object");
    let report = object_store.verify_all().expect("scan missing object row");
    assert!(
        report
            .invalid_control_records
            .contains(&"control_policy_state:active".into())
    );
    assert!(
        report
            .invalid_control_records
            .contains(&format!("control_policy_version:{active}"))
    );
}
