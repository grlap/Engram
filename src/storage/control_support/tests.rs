use chrono::{TimeDelta, TimeZone};

use super::*;
use crate::storage::{
    control_policy_version_load_count, reset_control_policy_version_load_count, test_support::*,
};
use crate::*;

use crate::{DevelopmentNoopRedactor, domain::ProjectPolicyEpoch};

#[test]
fn policy_operation_result_enforces_the_canonical_byte_limit() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open_in_memory().unwrap();
    let intent = CanonicalObject::freeze(&"result-size-boundary").unwrap();
    let transaction = store.connection.transaction().unwrap();
    // A JSON string adds two quote bytes to this ASCII payload.
    let at_limit = "x".repeat(MAX_CONTROL_POLICY_OPERATION_RESULT_BYTES - 2);
    assert_eq!(
        crate::canonical::canonical_bytes(&at_limit).unwrap().len(),
        MAX_CONTROL_POLICY_OPERATION_RESULT_BYTES
    );
    SqliteStore::persist_control_policy_operation(
        &transaction,
        "set_obligation_rule_set",
        "at-result-limit",
        &intent,
        &at_limit,
        now,
    )
    .expect("exact limit is admitted");
    let over_limit = format!("{at_limit}x");
    let error = SqliteStore::persist_control_policy_operation(
        &transaction,
        "set_obligation_rule_set",
        "over-result-limit",
        &intent,
        &over_limit,
        now,
    )
    .expect_err("one byte over the limit refuses");
    assert!(matches!(
        error,
        StoreError::InvalidControlProjection(message)
            if message == format!(
                "control policy operation result exceeds the {MAX_CONTROL_POLICY_OPERATION_RESULT_BYTES}-byte canonical limit"
            )
    ));
    let keys = transaction
        .prepare("SELECT idempotency_key FROM control_policy_operation_results")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(keys, ["at-result-limit"]);
    transaction.commit().unwrap();
    assert_projection_bytes(
        &store,
        "SELECT result_json FROM control_policy_operation_results
         WHERE operation = 'set_obligation_rule_set' AND idempotency_key = ?1",
        ["at-result-limit"],
        &at_limit,
    );
    let replayed = SqliteStore::replay_control_policy_operation::<String>(
        &store.connection,
        "set_obligation_rule_set",
        "at-result-limit",
        &intent,
    )
    .expect("exact limit is replayable");
    assert_eq!(replayed, Some(at_limit));

    // Simulate an oversized stored result to exercise the read-side guard
    // independently of the write-side refusal above.
    store
        .connection
        .execute(
            "UPDATE control_policy_operation_results SET result_json = ?1
             WHERE operation = 'set_obligation_rule_set' AND idempotency_key = ?2",
            params![
                crate::canonical::canonical_bytes(&over_limit).unwrap(),
                "at-result-limit"
            ],
        )
        .unwrap();
    let error = SqliteStore::replay_control_policy_operation::<String>(
        &store.connection,
        "set_obligation_rule_set",
        "at-result-limit",
        &intent,
    )
    .expect_err("one byte over the limit refuses replay too");
    assert!(matches!(
        error,
        StoreError::InvalidControlProjection(message)
            if message.starts_with("control policy operation result ")
                && message.ends_with(" exceeds its canonical byte limit")
    ));
}

#[test]
fn live_control_policy_load_is_bounded_independently_of_history_depth() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open_in_memory().expect("store");
    let mut active = store
        .control_diagnostics()
        .expect("initial diagnostics")
        .active_policy;
    for epoch in 2_i64..=51 {
        let rules = if epoch % 2 == 0 {
            crate::domain::ObligationRuleSet {
                schema_version: crate::OBLIGATION_RULE_SET_SCHEMA_VERSION,
                rules: Vec::new(),
            }
        } else {
            crate::control::builtin_obligation_rule_set()
        };
        let receipt = store
            .set_obligation_rule_set(
                &rules,
                &actor("policy-load-admin"),
                &format!("policy-load-{epoch}"),
                Some(&active),
                now + TimeDelta::milliseconds(epoch),
                &DevelopmentNoopRedactor,
            )
            .expect("extend policy history");
        assert_eq!(receipt.policy_epoch, ProjectPolicyEpoch(epoch));
        active = receipt.active_policy;
    }

    reset_control_policy_version_load_count();
    let loaded = SqliteStore::load_active_control_policy(&store.connection).unwrap();
    assert_eq!(loaded.policy_id, active);
    assert_eq!(control_policy_version_load_count(), 1);
}

/// The control policy head reads the state row, the version it selects and
/// whether a later version exists. A policy change committed between those
/// reads must not make the head look stale: the read stays on the policy it
/// began with.
#[test]
fn the_control_policy_head_reads_its_state_and_history_from_one_commit() {
    let directory = crate::test_support::temp_home().expect("temporary store directory");
    let database = directory.path().join("policy-race.db");
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut writer = SqliteStore::open(&database).expect("advisory store");
    let reader = SqliteStore::open(&database).expect("reader store");
    let (before, _, _) = SqliteStore::load_control_policy_head(&reader.connection).expect("before");
    let read = crate::storage::concurrent_commit::read_across_a_concurrent_commit(
        &reader,
        |reader| SqliteStore::load_control_policy_head(&reader.connection),
        &["FROM control_policy_versions WHERE policy_epoch > ?1"],
        move || {
            writer
                .set_obligation_rule_set(
                    &crate::domain::ObligationRuleSet {
                        schema_version: crate::OBLIGATION_RULE_SET_SCHEMA_VERSION,
                        rules: Vec::new(),
                    },
                    &actor("policy-race-admin"),
                    "policy-race",
                    None,
                    now,
                    &DevelopmentNoopRedactor,
                )
                .map(|_| ())
                .map_err(|error| error.to_string())
        },
    );
    let (read, _, _) = read.expect("one commit state, no false inconsistency");
    assert_eq!(read.epoch, before.epoch);
    assert_eq!(read.policy_id, before.policy_id);
    let (after, _, _) = SqliteStore::load_control_policy_head(&reader.connection).expect("after");
    assert_eq!(after.epoch, ProjectPolicyEpoch(before.epoch.0 + 1));
}
