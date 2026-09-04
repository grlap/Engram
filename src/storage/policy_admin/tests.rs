use chrono::{TimeDelta, TimeZone};
use rusqlite::Connection;

use super::*;
use crate::storage::{
    DIFFERENT_BUILD_STORE_MESSAGE, FAIL_COLD_SCHEMA_AFTER_DDL, MAX_CONTROL_POLICY_PROVENANCE_LINKS,
    test_support::*,
};
use crate::*;

use crate::{
    DevelopmentNoopRedactor,
    domain::{
        ControlAssurance, EffectClass, ProjectId, ProjectPolicyEpoch, ProvenanceLink,
        ProvenanceRelation, TurnIntent, TurnPurpose,
    },
};

#[test]
fn cold_schema_failure_after_ddl_rolls_back_every_control_table() {
    let directory = tempfile::tempdir().expect("temporary store directory");
    let database = directory.path().join("interrupted-cold-schema.db");
    FAIL_COLD_SCHEMA_AFTER_DDL.set(true);
    assert!(matches!(
        SqliteStore::open(&database),
        Err(StoreError::InvalidControlProjection(reason))
            if reason.contains("injected cold-schema failure")
    ));

    let raw = Connection::open(&database).expect("inspect rolled-back cold schema");
    for table in ["objects", "control_policy_state", "control_policy_versions"] {
        assert!(!SqliteStore::sqlite_table_exists(&raw, table).expect("inspect rolled-back table"));
    }
    drop(raw);
    drop(SqliteStore::open(&database).expect("retry cold bootstrap"));
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one restart fixture proves both policy setter receipts across conflicts, no-ops, later heads, and integrity scanning"
)]
fn policy_admin_receipts_replay_after_restart_and_later_policy_heads() {
    let directory = tempfile::tempdir().expect("temporary store directory");
    let database = directory.path().join("policy-operation-replay.db");
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = open_with_assurance(&database, ControlAssurance::Advisory)
        .expect("initialize advisory policy");
    let initial = store.control_diagnostics().expect("initial policy");
    let changed = store
        .set_required_control_assurance(
            ControlAssurance::TurnGated,
            &actor("durable-policy-admin"),
            "require durable turn mediation",
            "durable-assurance-update",
            Some(&initial.active_policy),
            now,
            &DevelopmentNoopRedactor,
        )
        .expect("activate policy");
    drop(store);

    let mut store = SqliteStore::open(&database).expect("reopen after uncertain response");
    let replay = store
        .set_required_control_assurance(
            ControlAssurance::TurnGated,
            &actor("durable-policy-admin"),
            "require durable turn mediation",
            "durable-assurance-update",
            Some(&initial.active_policy),
            now + TimeDelta::minutes(10),
            &DevelopmentNoopRedactor,
        )
        .expect("replay committed assurance receipt");
    assert_eq!(replay, changed);
    assert!(matches!(
        store.set_required_control_assurance(
            ControlAssurance::TurnGated,
            &actor("durable-policy-admin"),
            "different intent under the same key",
            "durable-assurance-update",
            Some(&initial.active_policy),
            now + TimeDelta::minutes(11),
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::ControlOperationIdempotencyConflict { .. })
    ));

    let no_op = store
        .set_required_control_assurance(
            ControlAssurance::TurnGated,
            &actor("durable-policy-admin"),
            "record an exact no-op receipt",
            "durable-assurance-noop",
            Some(&changed.active_policy),
            now + TimeDelta::minutes(12),
            &DevelopmentNoopRedactor,
        )
        .expect("record no-op receipt");
    assert!(!no_op.changed);
    drop(store);

    let mut store = SqliteStore::open(&database).expect("reopen no-op receipt");
    let no_op_replay = store
        .set_required_control_assurance(
            ControlAssurance::TurnGated,
            &actor("durable-policy-admin"),
            "record an exact no-op receipt",
            "durable-assurance-noop",
            Some(&changed.active_policy),
            now + TimeDelta::minutes(13),
            &DevelopmentNoopRedactor,
        )
        .expect("replay no-op receipt");
    assert_eq!(no_op_replay, no_op);

    let empty_rules = ObligationRuleSet {
        schema_version: OBLIGATION_RULE_SET_SCHEMA_VERSION,
        rules: Vec::new(),
    };
    let rule_changed = store
        .set_obligation_rule_set(
            &empty_rules,
            &actor("durable-rule-admin"),
            "select the empty obligation rule set",
            "durable-rule-update",
            Some(&changed.active_policy),
            now + TimeDelta::minutes(14),
            &DevelopmentNoopRedactor,
        )
        .expect("activate rule set");
    drop(store);

    let mut store = SqliteStore::open(&database).expect("reopen rule receipt");
    let rule_replay = store
        .set_obligation_rule_set(
            &empty_rules,
            &actor("durable-rule-admin"),
            "select the empty obligation rule set",
            "durable-rule-update",
            Some(&changed.active_policy),
            now + TimeDelta::minutes(15),
            &DevelopmentNoopRedactor,
        )
        .expect("replay rule-set receipt");
    assert_eq!(rule_replay, rule_changed);
    let later = store
        .set_required_control_assurance(
            ControlAssurance::Advisory,
            &actor("later-policy-admin"),
            "advance beyond the stored rule receipt",
            "later-assurance-update",
            Some(&rule_changed.active_policy),
            now + TimeDelta::minutes(16),
            &DevelopmentNoopRedactor,
        )
        .expect("activate later policy head");
    assert_eq!(later.policy_epoch.0, rule_changed.policy_epoch.0 + 1);
    let replay_after_later_head = store
        .set_obligation_rule_set(
            &empty_rules,
            &actor("durable-rule-admin"),
            "select the empty obligation rule set",
            "durable-rule-update",
            Some(&changed.active_policy),
            now + TimeDelta::minutes(17),
            &DevelopmentNoopRedactor,
        )
        .expect("replay rule receipt after later head");
    assert_eq!(replay_after_later_head, rule_changed);
    assert!(matches!(
        store.set_obligation_rule_set(
            &empty_rules,
            &actor("durable-rule-admin"),
            "different rule intent under the same key",
            "durable-rule-update",
            Some(&changed.active_policy),
            now + TimeDelta::minutes(18),
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::ControlOperationIdempotencyConflict { .. })
    ));
    assert!(
        store
            .verify_all()
            .expect("verify durable receipts")
            .is_healthy()
    );
    let corrupted_sequence = store
        .connection
        .query_row(
            "SELECT sequence FROM control_policy_operation_results
             WHERE operation = 'set_required_assurance'
               AND idempotency_key = 'durable-assurance-update'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("locate durable receipt");
    store
        .connection
        .execute(
            "UPDATE control_policy_operation_results SET result_json = ?1
             WHERE sequence = ?2",
            params![b"{}".as_slice(), corrupted_sequence],
        )
        .expect("corrupt durable receipt projection");
    assert!(
        store
            .verify_all()
            .expect("scan corrupt durable receipt")
            .invalid_control_records
            .contains(&format!("control_policy_operation:{corrupted_sequence}"))
    );
}

#[test]
fn failed_policy_receipt_insert_rolls_back_the_policy_activation() {
    let directory = tempfile::tempdir().expect("temporary store directory");
    let database = directory.path().join("policy-operation-rollback.db");
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open(&database).expect("initialize store");
    let initial = store.control_diagnostics().expect("initial policy");
    store
        .connection
        .execute_batch(
            "CREATE TRIGGER fail_policy_operation_receipt
             BEFORE INSERT ON control_policy_operation_results
             BEGIN
                 SELECT RAISE(ABORT, 'injected policy receipt failure');
             END;",
        )
        .expect("install receipt failure trigger");
    assert!(matches!(
        store.set_required_control_assurance(
            ControlAssurance::Advisory,
            &actor("rollback-policy-admin"),
            "prove receipt and activation share one transaction",
            "rollback-policy-update",
            Some(&initial.active_policy),
            now,
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::Sqlite(_))
    ));
    store
        .connection
        .execute_batch("DROP TRIGGER fail_policy_operation_receipt;")
        .expect("drop receipt failure trigger");
    let after = store.control_diagnostics().expect("policy after rollback");
    assert_eq!(after.active_policy, initial.active_policy);
    assert_eq!(after.policy_epoch, initial.policy_epoch);
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM control_policy_operation_results",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count policy receipts"),
        0
    );
    store
        .connection
        .execute_batch(
            "CREATE TRIGGER fail_policy_operation_receipt
             BEFORE INSERT ON control_policy_operation_results
             BEGIN
                 SELECT RAISE(ABORT, 'injected rule receipt failure');
             END;",
        )
        .expect("reinstall receipt failure trigger");
    let empty_rules = ObligationRuleSet {
        schema_version: OBLIGATION_RULE_SET_SCHEMA_VERSION,
        rules: Vec::new(),
    };
    assert!(matches!(
        store.set_obligation_rule_set(
            &empty_rules,
            &actor("rollback-rule-admin"),
            "prove rule receipt and activation share one transaction",
            "rollback-rule-update",
            Some(&initial.active_policy),
            now + TimeDelta::seconds(1),
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::Sqlite(_))
    ));
    store
        .connection
        .execute_batch("DROP TRIGGER fail_policy_operation_receipt;")
        .expect("drop rule receipt failure trigger");
    let after_rule = store
        .control_diagnostics()
        .expect("policy after rule rollback");
    assert_eq!(after_rule.active_policy, initial.active_policy);
    assert_eq!(after_rule.obligation_rule_set, initial.obligation_rule_set);
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM control_policy_operation_results",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count rule receipts"),
        0
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one restart fixture keeps bootstrap attribution, CAS, corruption, and reopen behavior on the same immutable chain"
)]
fn control_policy_versions_are_canonical_idempotent_and_restart_safe() {
    let directory = tempfile::tempdir().expect("temporary store directory");
    let database = directory.path().join("engram.db");
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = open_with_assurance(&database, ControlAssurance::Advisory)
        .expect("initialize advisory policy");
    let initial = store.control_diagnostics().expect("initial diagnostics");
    assert_eq!(initial.required_assurance, ControlAssurance::Advisory);
    assert_eq!(initial.policy_epoch, ProjectPolicyEpoch(1));

    let initial_policy: ControlPolicy = store
        .get(&initial.active_policy)
        .expect("read initial policy")
        .expect("initial policy object");
    assert_eq!(initial_policy.previous_policy, None);
    let initial_authority: ProjectPolicyAuthorityDecision = store
        .get(&initial_policy.authority)
        .expect("read initial authority")
        .expect("initial authority object");
    assert_eq!(
        initial_authority.authorized_by.actor_id,
        "bootstrap-policy-admin"
    );
    assert_eq!(initial_authority.reason, "select the test bootstrap policy");

    let changed = store
        .set_required_control_assurance(
            ControlAssurance::TurnGated,
            &actor("policy-admin"),
            "require host turn mediation",
            "policy-turn-gated",
            Some(&initial.active_policy),
            now,
            &DevelopmentNoopRedactor,
        )
        .expect("activate turn-gated policy");
    assert!(changed.changed);
    assert_eq!(changed.previous_policy, Some(initial.active_policy.clone()));
    assert_eq!(changed.policy_epoch, ProjectPolicyEpoch(2));
    assert_eq!(changed.required_assurance, ControlAssurance::TurnGated);

    let replay = store
        .set_required_control_assurance(
            ControlAssurance::TurnGated,
            &actor("policy-admin"),
            "idempotent replay",
            "policy-turn-gated-noop",
            Some(&changed.active_policy),
            now + TimeDelta::seconds(1),
            &DevelopmentNoopRedactor,
        )
        .expect("reapply active policy");
    assert!(!replay.changed);
    assert_eq!(replay.active_policy, changed.active_policy);
    assert_eq!(replay.policy_epoch, changed.policy_epoch);
    assert!(matches!(
        store.set_required_control_assurance(
            ControlAssurance::ActionGated,
            &actor("policy-admin"),
            "stale compare and swap",
            "policy-stale-cas",
            Some(&initial.active_policy),
            now + TimeDelta::seconds(2),
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::ControlPolicyConflict { .. })
    ));
    assert!(
        store
            .verify_all()
            .expect("verify policy history")
            .is_healthy()
    );
    drop(store);

    let reopened = SqliteStore::open(&database).expect("reopen configured store");
    let diagnostics = reopened
        .control_diagnostics()
        .expect("reopened diagnostics");
    assert_eq!(diagnostics.active_policy, changed.active_policy);
    assert_eq!(diagnostics.policy_epoch, ProjectPolicyEpoch(2));
    assert_eq!(diagnostics.required_assurance, ControlAssurance::TurnGated);
    drop(reopened);
    assert!(matches!(
        open_with_assurance(&database, ControlAssurance::Advisory),
        Err(StoreError::InvalidControlProjection(_))
    ));

    let corrupted_store = SqliteStore::open(&database).expect("reopen corruption fixture");
    corrupted_store
        .connection
        .execute(
            "UPDATE control_policy_versions SET policy_json = ?1
             WHERE policy_hash = ?2",
            params![b"{}".as_slice(), changed.active_policy.as_str()],
        )
        .expect("corrupt active policy projection");
    let corrupted = corrupted_store.verify_all().expect("scan corrupt policy");
    assert!(
        corrupted
            .invalid_control_records
            .contains(&"control_policy_state:active".into())
    );
    assert!(
        corrupted
            .invalid_control_records
            .contains(&format!("control_policy_version:{}", changed.active_policy))
    );
    drop(corrupted_store);
    assert!(SqliteStore::open(&database).is_err());
}

#[test]
fn obligation_rule_set_activation_is_canonical_idempotent_and_restart_safe() {
    let directory = tempfile::tempdir().expect("temporary store directory");
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
            "disable future obligation triggers",
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
            "exact semantic replay",
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
            "exact semantic replay",
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
            "unknown schema must fail closed",
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
fn established_store_missing_policy_state_refuses_without_bootstrap() {
    let directory = tempfile::tempdir().expect("temporary store directory");

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
        Err(StoreError::InvalidControlProjection(reason))
            if reason == DIFFERENT_BUILD_STORE_MESSAGE
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
            "UPDATE control_policy_state SET policy_hash = NULL WHERE singleton = 1",
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
    let directory = tempfile::tempdir().expect("temporary store directory");
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
        Err(StoreError::InvalidControlProjection(reason))
            if reason == DIFFERENT_BUILD_STORE_MESSAGE
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
            "SELECT object_kind FROM objects WHERE object_hash = ?1",
            [ordinary.hash().as_str()],
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
fn active_policy_must_be_the_unique_maximal_history_head() {
    let directory = tempfile::tempdir().expect("temporary store directory");
    let database = directory.path().join("engram.db");
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = open_with_assurance(&database, ControlAssurance::Advisory)
        .expect("initialize policy history");
    let initial = store.control_diagnostics().expect("initial diagnostics");
    let initial_policy: ControlPolicy = store
        .get(&initial.active_policy)
        .expect("read initial policy")
        .expect("initial policy object");
    let changed = store
        .set_required_control_assurance(
            ControlAssurance::TurnGated,
            &actor("policy-admin"),
            "create a successor",
            "policy-successor-rollback",
            Some(&initial.active_policy),
            now,
            &DevelopmentNoopRedactor,
        )
        .expect("activate successor policy");
    let projected_assurance =
        enum_name(initial_policy.required_assurance).expect("serialize assurance");
    let projected_effects =
        serde_json::to_string(&initial_policy.supported_effects).expect("serialize effects");
    store
        .connection
        .execute(
            "UPDATE control_policy_state SET
                 policy_epoch = ?1, required_assurance = ?2,
                 supported_effects_json = ?3, grant_ttl_seconds = ?4,
                 policy_hash = ?5
             WHERE singleton = 1",
            params![
                initial_policy.policy_epoch.0,
                projected_assurance,
                projected_effects,
                initial_policy.grant_ttl_seconds,
                initial.active_policy.as_str(),
            ],
        )
        .expect("simulate selector rollback");

    let integrity = store.verify_all().expect("scan rolled-back selector");
    assert!(
        integrity
            .invalid_control_records
            .contains(&"control_policy_state:active".into())
    );
    assert!(
        integrity
            .invalid_control_records
            .contains(&format!("control_policy_version:{}", changed.active_policy))
    );
    drop(store);
    assert!(matches!(
        SqliteStore::open(&database),
        Err(StoreError::InvalidControlProjection(reason))
            if reason.contains("maximal history head")
    ));
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

#[test]
fn control_diagnostics_counts_issued_grants_at_the_injected_instant() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open_in_memory().expect("store");
    let binding = bind_control(&mut store, now);
    complete_control_turn(
        &mut store,
        &binding,
        "diagnostic-sync",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(1),
    );
    let decision = store
        .evaluate_control_turn(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &TurnIntent {
                idempotency_key: "diagnostic-issued".into(),
                intent_fingerprint: ObjectHash::from_canonical_bytes(b"diagnostic-issued"),
                purpose: TurnPurpose::Ordinary,
                requested_effects: vec![EffectClass::Observe],
                resource_intents: Vec::new(),
            },
            now + TimeDelta::seconds(2),
        )
        .expect("issue diagnostic grant");
    let ControlTurnDecision::Grant { grant } = decision else {
        panic!("diagnostic fixture must grant");
    };
    assert_eq!(
        store
            .control_diagnostics_at(grant.basis.expires_at - TimeDelta::milliseconds(1))
            .expect("diagnostics before expiry")
            .issued_turns,
        1
    );
    assert_eq!(
        store
            .control_diagnostics_at(grant.basis.expires_at)
            .expect("diagnostics at expiry")
            .issued_turns,
        0
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the corruption fixture must rebind both policy versions and the successor authority"
)]
fn set_required_assurance_history_cannot_change_effects_or_ttl() {
    let directory = tempfile::tempdir().expect("temporary store directory");
    let database = directory.path().join("engram.db");
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = open_with_assurance(&database, ControlAssurance::Advisory)
        .expect("initialize policy history");
    let initial = store.control_diagnostics().expect("initial diagnostics");
    let mut historical_policy: ControlPolicy = store
        .get(&initial.active_policy)
        .expect("read initial policy")
        .expect("initial policy object");
    let changed = store
        .set_required_control_assurance(
            ControlAssurance::TurnGated,
            &actor("policy-admin"),
            "create current policy",
            "policy-current-envelope",
            Some(&initial.active_policy),
            now,
            &DevelopmentNoopRedactor,
        )
        .expect("activate current policy");
    let mut active_policy: ControlPolicy = store
        .get(&changed.active_policy)
        .expect("read active policy")
        .expect("active policy object");
    let mut active_authority: ProjectPolicyAuthorityDecision = store
        .get(&changed.authority)
        .expect("read active authority")
        .expect("active authority object");

    historical_policy.supported_effects = vec![EffectClass::Observe];
    historical_policy.grant_ttl_seconds -= 1;
    let historical_object =
        CanonicalObject::freeze(&historical_policy).expect("freeze corrupted history");
    active_authority.previous_policy = Some(historical_object.hash().clone());
    let active_authority_object =
        CanonicalObject::freeze(&active_authority).expect("freeze rebound authority");
    active_policy.previous_policy = Some(historical_object.hash().clone());
    active_policy.authority = active_authority_object.hash().clone();
    let active_policy_object =
        CanonicalObject::freeze(&active_policy).expect("freeze rebound active policy");

    let transaction = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("begin corrupt history replacement");
    SqliteStore::insert_object(&transaction, "control_policy", &historical_object)
        .expect("insert corrupted historical policy");
    SqliteStore::insert_object(
        &transaction,
        "project_policy_authority_decision",
        &active_authority_object,
    )
    .expect("insert rebound authority");
    SqliteStore::insert_object(&transaction, "control_policy", &active_policy_object)
        .expect("insert rebound active policy");
    transaction
        .execute("DELETE FROM control_policy_versions", [])
        .expect("replace projected history");
    transaction
        .execute(
            "INSERT INTO control_policy_versions (
                 policy_hash, policy_epoch, authority_hash, policy_json
             ) VALUES (?1, ?2, ?3, ?4), (?5, ?6, ?7, ?8)",
            params![
                historical_object.hash().as_str(),
                historical_policy.policy_epoch.0,
                historical_policy.authority.as_str(),
                historical_object.bytes(),
                active_policy_object.hash().as_str(),
                active_policy.policy_epoch.0,
                active_authority_object.hash().as_str(),
                active_policy_object.bytes(),
            ],
        )
        .expect("install corrupt history");
    transaction
        .execute(
            "UPDATE control_policy_state SET policy_hash = ?1 WHERE singleton = 1",
            [active_policy_object.hash().as_str()],
        )
        .expect("select corrupt active head");
    transaction.commit().expect("commit corrupt history");

    let integrity = store.verify_all().expect("scan corrupt history");
    assert!(
        integrity
            .invalid_control_records
            .contains(&"control_policy_state:active".into())
    );
    drop(store);
    assert!(matches!(
        SqliteStore::open(&database),
        Err(StoreError::InvalidControlProjection(_))
    ));
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one table-driven boundary test keeps every nested attribution leaf and both size limits visibly covered"
)]
fn policy_administrator_attribution_is_fully_inspected_and_bounded() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open_in_memory().expect("store");
    let mut actors = Vec::new();
    for field in ["run", "session", "tool", "skill", "source", "reference"] {
        let mut candidate = actor("policy-admin");
        match field {
            "run" => candidate.run_id = Some("reject-me".into()),
            "session" => candidate.session_id = Some(SessionId("reject-me".into())),
            "tool" => candidate.source_tool = Some("reject-me".into()),
            "skill" => candidate.source_skill = Some("reject-me".into()),
            "source" => candidate.provenance_chain.push(ProvenanceLink {
                relation: ProvenanceRelation::AssertedBy,
                source: "reject-me".into(),
                reference: None,
            }),
            "reference" => candidate.provenance_chain.push(ProvenanceLink {
                relation: ProvenanceRelation::AssertedBy,
                source: "test-source".into(),
                reference: Some("reject-me".into()),
            }),
            _ => unreachable!("enumerated attribution field"),
        }
        actors.push(candidate);
    }
    for candidate in actors {
        assert!(matches!(
            store.set_required_control_assurance(
                ControlAssurance::Advisory,
                &candidate,
                "inspect every attribution leaf",
                "policy-redaction-leaf",
                None,
                now,
                &SentinelRedactor,
            ),
            Err(StoreError::RedactionRefused(_))
        ));
    }

    let mut oversized_field = actor("policy-admin");
    oversized_field.source_skill = Some("x".repeat(4_097));
    assert!(matches!(
        store.set_required_control_assurance(
            ControlAssurance::Advisory,
            &oversized_field,
            "bound optional fields",
            "policy-oversized-field",
            None,
            now,
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::InvalidControlProjection(_))
    ));

    let mut too_many_links = actor("policy-admin");
    too_many_links.provenance_chain = (0..=MAX_CONTROL_POLICY_PROVENANCE_LINKS)
        .map(|index| ProvenanceLink {
            relation: ProvenanceRelation::DerivedFrom,
            source: format!("source-{index}"),
            reference: None,
        })
        .collect();
    assert!(matches!(
        store.set_required_control_assurance(
            ControlAssurance::Advisory,
            &too_many_links,
            "bound provenance count",
            "policy-provenance-count",
            None,
            now,
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::InvalidControlProjection(_))
    ));

    let mut oversized_attribution = actor("policy-admin");
    oversized_attribution.provenance_chain = (0..MAX_CONTROL_POLICY_PROVENANCE_LINKS)
        .map(|index| ProvenanceLink {
            relation: ProvenanceRelation::RelayedBy,
            source: format!("source-{index}-{}", "s".repeat(2_000)),
            reference: Some(format!("reference-{index}-{}", "r".repeat(2_000))),
        })
        .collect();
    assert!(matches!(
        store.set_required_control_assurance(
            ControlAssurance::Advisory,
            &oversized_attribution,
            "bound aggregate attribution",
            "policy-oversized-attribution",
            None,
            now,
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::InvalidControlProjection(_))
    ));

    let mut normalized = actor(" policy-admin ");
    normalized.run_id = Some(" run-1 ".into());
    normalized.session_id = Some(SessionId(" session-1 ".into()));
    normalized.source_tool = Some(" host-tool ".into());
    normalized.source_skill = Some(" control-skill ".into());
    normalized.provenance_chain = vec![ProvenanceLink {
        relation: ProvenanceRelation::AssertedBy,
        source: " host-observation ".into(),
        reference: Some(" receipt-1 ".into()),
    }];
    let receipt = store
        .set_required_control_assurance(
            ControlAssurance::Advisory,
            &normalized,
            " normalize persisted attribution ",
            "policy-normalized-attribution",
            None,
            now,
            &DevelopmentNoopRedactor,
        )
        .expect("persist normalized attribution");
    let authority: ProjectPolicyAuthorityDecision = store
        .get(&receipt.authority)
        .expect("read authority")
        .expect("authority object");
    assert_eq!(authority.authorized_by.actor_id, "policy-admin");
    assert_eq!(authority.authorized_by.run_id.as_deref(), Some("run-1"));
    assert_eq!(
        authority.authorized_by.session_id,
        Some(SessionId("session-1".into()))
    );
    assert_eq!(
        authority.authorized_by.provenance_chain[0]
            .reference
            .as_deref(),
        Some("receipt-1")
    );
    assert_eq!(authority.reason, "normalize persisted attribution");
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one lifecycle fixture keeps issued and begun grant behavior visibly comparable across the same policy transition"
)]
fn policy_epoch_change_expires_issued_grants_but_not_begun_checkpoints() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open_in_memory().expect("store");
    let begun = bind_control_for(
        &mut store,
        "policy-begun",
        "bind-policy-begun",
        &[EffectClass::Observe],
        now,
    );
    let issued = bind_control_for(
        &mut store,
        "policy-issued",
        "bind-policy-issued",
        &[EffectClass::Observe],
        now,
    );
    let evaluate =
        |store: &mut SqliteStore, binding: &TestControlBinding, key: &str, at: DateTime<Utc>| {
            store
                .evaluate_control_turn(
                    &ProjectId("project-a".into()),
                    &binding.status.session_id,
                    &binding.connection_token,
                    &binding.routing_token,
                    &TurnIntent {
                        idempotency_key: key.into(),
                        intent_fingerprint: ObjectHash::from_canonical_bytes(key.as_bytes()),
                        purpose: TurnPurpose::Ordinary,
                        requested_effects: vec![EffectClass::Observe],
                        resource_intents: Vec::new(),
                    },
                    at,
                )
                .expect("evaluate controlled turn")
        };
    let ControlTurnDecision::Grant { grant: begun_grant } =
        evaluate(&mut store, &begun, "policy-begun-turn", now)
    else {
        panic!("begun fixture must grant");
    };
    let begun_tokens = begun_grant
        .delivery
        .iter()
        .map(|delivery| delivery.page.delivery_token.clone())
        .collect::<Vec<_>>();
    assert!(matches!(
        store
            .begin_control_turn(
                &ProjectId("project-a".into()),
                &begun.status.session_id,
                &begun.connection_token,
                &begun.routing_token,
                &begun_grant.grant_id,
                &begun_tokens,
                "begin-before-policy-change",
                now + TimeDelta::milliseconds(1),
            )
            .expect("begin pre-change grant"),
        ControlTurnBeginDecision::Begin { .. }
    ));
    let ControlTurnDecision::Grant {
        grant: issued_grant,
    } = evaluate(
        &mut store,
        &issued,
        "policy-issued-turn",
        now + TimeDelta::milliseconds(2),
    )
    else {
        panic!("issued fixture must grant");
    };

    let changed = store
        .set_required_control_assurance(
            ControlAssurance::Advisory,
            &actor("policy-admin"),
            "exercise policy epoch transition",
            "policy-epoch-transition",
            None,
            now + TimeDelta::seconds(1),
            &DevelopmentNoopRedactor,
        )
        .expect("activate new policy");
    assert_eq!(changed.policy_epoch, ProjectPolicyEpoch(2));
    assert!(matches!(
        store
            .checkpoint_control_turn(
                &ProjectId("project-a".into()),
                &begun.status.session_id,
                &begun.connection_token,
                &begun.routing_token,
                &begun_grant.grant_id,
                TurnNextIntent::Continue,
                "checkpoint-after-policy-change",
                now + TimeDelta::seconds(2),
            )
            .expect("checkpoint begun grant"),
        ControlTurnCheckpointDecision::Checkpointed { .. }
    ));
    assert!(matches!(
        evaluate(
            &mut store,
            &begun,
            "policy-refresh-after-checkpoint",
            now + TimeDelta::milliseconds(2_100),
        ),
        ControlTurnDecision::Refuse {
            directive: crate::domain::ControlDirective {
                code: crate::domain::ControlRefusalCode::PolicyEpochChanged,
                ..
            }
        }
    ));
    assert_eq!(
        store
            .control_status(
                &ProjectId("project-a".into()),
                &begun.status.session_id,
                &begun.connection_token,
                &begun.routing_token,
                now + TimeDelta::milliseconds(2_100),
            )
            .expect("status after evaluation epoch refresh")
            .epochs
            .project_policy,
        ProjectPolicyEpoch(2)
    );
    assert!(matches!(
        evaluate(
            &mut store,
            &begun,
            "policy-turn-after-refresh",
            now + TimeDelta::milliseconds(2_200),
        ),
        ControlTurnDecision::Grant { .. }
    ));
    assert!(matches!(
        store
            .begin_control_turn(
                &ProjectId("project-a".into()),
                &issued.status.session_id,
                &issued.connection_token,
                &issued.routing_token,
                &issued_grant.grant_id,
                &[],
                "begin-issued-after-policy-change",
                now + TimeDelta::seconds(2),
            )
            .expect("refuse stale issued grant"),
        ControlTurnBeginDecision::Refuse {
            code: crate::domain::ControlRefusalCode::PolicyEpochChanged
        }
    ));
    assert_eq!(
        store
            .control_status(
                &ProjectId("project-a".into()),
                &issued.status.session_id,
                &issued.connection_token,
                &issued.routing_token,
                now + TimeDelta::seconds(2),
            )
            .expect("status after epoch refusal")
            .epochs
            .project_policy,
        ProjectPolicyEpoch(2)
    );
    assert!(matches!(
        evaluate(
            &mut store,
            &issued,
            "policy-issued-re-evaluated",
            now + TimeDelta::seconds(3),
        ),
        ControlTurnDecision::Grant { .. }
    ));
}

#[test]
fn action_gated_requirement_refuses_every_v1_host_fail_closed() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open_in_memory().expect("store");
    let turn_gated = bind_control_for(
        &mut store,
        "turn-gated-under-action-policy",
        "bind-before-action-policy",
        &[EffectClass::Observe],
        now,
    );
    let current = store.control_diagnostics().expect("current policy");
    store
        .set_required_control_assurance(
            ControlAssurance::ActionGated,
            &actor("action-policy-admin"),
            "prove the unavailable assurance fails closed",
            "policy-action-gated",
            Some(&current.active_policy),
            now + TimeDelta::seconds(1),
            &DevelopmentNoopRedactor,
        )
        .expect("activate action-gated requirement");

    let action_floor_lease = store
        .acquire_work_lease(
            &ProjectId("project-a".into()),
            &turn_gated.status.session_id,
            &turn_gated.connection_token,
            &turn_gated.routing_token,
            crate::domain::LeaseKind::Execution,
            crate::domain::LeaseMode::Exclusive,
            &crate::domain::ResourceSubject::Path {
                project_id: ProjectId("project-a".into()),
                segments: vec!["src".into()],
                coverage: crate::domain::ResourceCoverage::Tree,
            },
            60,
            "lease-under-action-policy",
            now + TimeDelta::seconds(2),
        )
        .expect("action-gated project floor is a lease decision");
    assert!(matches!(
        action_floor_lease,
        WorkLeaseDecision::Refuse { directive }
            if directive.code == ControlRefusalCode::ControlAssuranceInsufficient
                && directive.effect.is_none()
                && directive.required_assurance == Some(ControlAssurance::ActionGated)
    ));

    assert!(matches!(
        store
            .evaluate_control_turn(
                &ProjectId("project-a".into()),
                &turn_gated.status.session_id,
                &turn_gated.connection_token,
                &turn_gated.routing_token,
                &TurnIntent {
                    idempotency_key: "turn-under-action-policy".into(),
                    intent_fingerprint: ObjectHash::from_canonical_bytes(
                        b"turn-under-action-policy",
                    ),
                    purpose: TurnPurpose::Ordinary,
                    requested_effects: vec![EffectClass::Observe],
                    resource_intents: Vec::new(),
                },
                now + TimeDelta::seconds(2),
            )
            .expect("evaluate turn-gated host"),
        ControlTurnDecision::Refuse {
            directive: crate::domain::ControlDirective {
                code: crate::domain::ControlRefusalCode::ControlAssuranceInsufficient,
                ..
            }
        }
    ));

    let action_session = SessionId("action-gated-host".into());
    let connection_token = store
        .resume_control_connection(&action_session, now + TimeDelta::seconds(3))
        .expect("resume action-gated host connection");
    assert!(matches!(
        store.bind_control_session(
            &ProjectId("project-a".into()),
            "dummy:ACTION-GATED-HOST",
            "V1 must reject an action-gated declaration",
            &action_session,
            &connection_token,
            &actor("action-gated-host"),
            ControlAssurance::ActionGated,
            &[EffectClass::Observe],
            1,
            "bind-action-gated-host",
            now + TimeDelta::seconds(3),
        ),
        Err(StoreError::InvalidControlSession(reason))
            if reason.contains("bind fields")
    ));
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the lease epoch fixture keeps the refused replay and adopted fresh-key path adjacent"
)]
fn lease_epoch_refusal_is_sticky_and_adopts_for_a_fresh_key() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open_in_memory().expect("store");
    let binding = bind_control_for(
        &mut store,
        "lease-epoch-host",
        "bind-lease-epoch-host",
        &[EffectClass::Observe, EffectClass::MutateLocal],
        now,
    );
    complete_control_turn(
        &mut store,
        &binding,
        "sync-lease-epoch-host",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(1),
    );
    let current = store.control_diagnostics().expect("current policy");
    store
        .set_required_control_assurance(
            ControlAssurance::Advisory,
            &actor("lease-epoch-admin"),
            "exercise lease epoch adoption",
            "policy-lease-epoch",
            Some(&current.active_policy),
            now + TimeDelta::seconds(2),
            &DevelopmentNoopRedactor,
        )
        .expect("activate epoch two");
    let subject = crate::domain::ResourceSubject::Path {
        project_id: ProjectId("project-a".into()),
        segments: vec!["src".into()],
        coverage: crate::domain::ResourceCoverage::Tree,
    };

    let stale = store
        .acquire_work_lease(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            crate::domain::LeaseKind::Execution,
            crate::domain::LeaseMode::Exclusive,
            &subject,
            60,
            "stale-epoch-lease",
            now + TimeDelta::seconds(3),
        )
        .expect("stale epoch is a lease decision");
    assert!(matches!(
        &stale,
        WorkLeaseDecision::Refuse { directive }
            if directive.code == ControlRefusalCode::PolicyEpochChanged
    ));
    assert_eq!(
        store
            .control_status(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                now + TimeDelta::seconds(3),
            )
            .expect("status after lease epoch refusal")
            .epochs
            .project_policy,
        ProjectPolicyEpoch(2)
    );
    assert_eq!(
        store
            .acquire_work_lease(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &subject,
                60,
                "stale-epoch-lease",
                now + TimeDelta::seconds(4),
            )
            .expect("sticky stale-epoch replay"),
        stale
    );
    assert!(matches!(
        store
            .acquire_work_lease(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &subject,
                60,
                "fresh-epoch-lease",
                now + TimeDelta::seconds(5),
            )
            .expect("fresh key re-evaluates after epoch adoption"),
        WorkLeaseDecision::Granted { .. }
    ));
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one persisted lifecycle test pins bind caps, replayable lease refusal, and turn admission together"
)]
fn advisory_effect_floor_refuses_mutation_and_execution_lease() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("engram.db");
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store =
        open_with_assurance(&database, ControlAssurance::Advisory).expect("advisory store");
    let session_id = SessionId("advisory-effect-host".into());
    let connection_token = store
        .resume_control_connection(&session_id, now)
        .expect("resume advisory connection");
    let binding = store
        .bind_control_session(
            &ProjectId("project-a".into()),
            "dummy:ADVISORY-EFFECT-HOST",
            "Prove effect-specific assurance floors",
            &session_id,
            &connection_token,
            &actor("advisory-effect-host"),
            ControlAssurance::Advisory,
            &[
                EffectClass::Observe,
                EffectClass::Communicate,
                EffectClass::MutateLocal,
            ],
            1,
            "bind-advisory-effect-host",
            now,
        )
        .expect("bind advisory host");
    assert_eq!(
        binding.effective_mediated_effects,
        vec![EffectClass::Observe, EffectClass::Communicate]
    );
    assert!(
        binding
            .status
            .mediated_effects
            .contains(&EffectClass::MutateLocal)
    );
    let advisory = TestControlBinding {
        binding,
        connection_token,
    };
    complete_control_turn(
        &mut store,
        &advisory,
        "sync-advisory-effect-host",
        vec![EffectClass::Observe, EffectClass::Communicate],
        Vec::new(),
        now + TimeDelta::seconds(1),
    );
    let subject = crate::domain::ResourceSubject::Path {
        project_id: ProjectId("project-a".into()),
        segments: vec!["src".into()],
        coverage: crate::domain::ResourceCoverage::Tree,
    };
    let lease_refusal = store
        .acquire_work_lease(
            &ProjectId("project-a".into()),
            &advisory.status.session_id,
            &advisory.connection_token,
            &advisory.routing_token,
            crate::domain::LeaseKind::Execution,
            crate::domain::LeaseMode::Exclusive,
            &subject,
            60,
            "lease-advisory-effect-host",
            now + TimeDelta::seconds(2),
        )
        .expect("policy refusal is a lease decision");
    assert!(matches!(
        &lease_refusal,
        WorkLeaseDecision::Refuse { directive }
            if directive.code
                == crate::domain::ControlRefusalCode::ControlAssuranceInsufficient
                && directive.effect == Some(EffectClass::MutateLocal)
                && directive.required_assurance == Some(ControlAssurance::TurnGated)
    ));
    assert_eq!(
        store
            .acquire_work_lease(
                &ProjectId("project-a".into()),
                &advisory.status.session_id,
                &advisory.connection_token,
                &advisory.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &subject,
                60,
                "lease-advisory-effect-host",
                now + TimeDelta::seconds(3),
            )
            .expect("replay policy refusal"),
        lease_refusal
    );
    let decision = store
        .evaluate_control_turn(
            &ProjectId("project-a".into()),
            &advisory.status.session_id,
            &advisory.connection_token,
            &advisory.routing_token,
            &TurnIntent {
                idempotency_key: "mutate-under-advisory-assurance".into(),
                intent_fingerprint: ObjectHash::from_canonical_bytes(
                    b"mutate-under-advisory-assurance",
                ),
                purpose: TurnPurpose::Ordinary,
                requested_effects: vec![EffectClass::MutateLocal],
                resource_intents: vec![subject.clone()],
            },
            now + TimeDelta::seconds(4),
        )
        .expect("evaluate advisory mutation");
    let ControlTurnDecision::Refuse { directive } = decision else {
        panic!("advisory mutation must refuse");
    };
    assert_eq!(
        directive.code,
        crate::domain::ControlRefusalCode::ControlAssuranceInsufficient
    );
    assert_eq!(directive.effect, Some(EffectClass::MutateLocal));
    assert_eq!(
        directive.required_assurance,
        Some(ControlAssurance::TurnGated)
    );

    let turn_gated = bind_control_for_task(
        &mut store,
        "turn-gated-effect-host",
        "bind-turn-gated-effect-host",
        "dummy:ADVISORY-EFFECT-HOST",
        &[EffectClass::Observe, EffectClass::MutateLocal],
        now + TimeDelta::seconds(5),
    );
    complete_control_turn(
        &mut store,
        &turn_gated,
        "sync-turn-gated-effect-host",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(6),
    );
    let WorkLeaseDecision::Granted { .. } = store
        .acquire_work_lease(
            &ProjectId("project-a".into()),
            &turn_gated.status.session_id,
            &turn_gated.connection_token,
            &turn_gated.routing_token,
            crate::domain::LeaseKind::Execution,
            crate::domain::LeaseMode::Exclusive,
            &subject,
            60,
            "lease-turn-gated-effect-host",
            now + TimeDelta::seconds(7),
        )
        .expect("turn-gated execution lease")
    else {
        panic!("turn-gated host must acquire an execution lease");
    };
    assert!(matches!(
        store
            .evaluate_control_turn(
                &ProjectId("project-a".into()),
                &turn_gated.status.session_id,
                &turn_gated.connection_token,
                &turn_gated.routing_token,
                &TurnIntent {
                    idempotency_key: "mutate-under-turn-gated-assurance".into(),
                    intent_fingerprint: ObjectHash::from_canonical_bytes(
                        b"mutate-under-turn-gated-assurance",
                    ),
                    purpose: TurnPurpose::Ordinary,
                    requested_effects: vec![EffectClass::MutateLocal],
                    resource_intents: vec![subject],
                },
                now + TimeDelta::seconds(8),
            )
            .expect("evaluate turn-gated mutation"),
        ControlTurnDecision::Grant { .. }
    ));
}
