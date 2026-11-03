use chrono::TimeDelta;

use super::*;
use crate::domain::*;
use crate::{CanonicalObject, ObjectId};

fn replace_policy(store: &SqliteStore, id: &ObjectId, policy: &ControlPolicy) {
    let object = CanonicalObject::identified(id, policy).unwrap();
    store
        .connection
        .execute(
            "UPDATE objects SET canonical_json = ?1 WHERE object_id = ?2",
            rusqlite::params![object.bytes(), id.as_str()],
        )
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE control_policy_versions SET policy_json = ?1 WHERE policy_id = ?2",
            rusqlite::params![object.bytes(), id.as_str()],
        )
        .unwrap();
}

fn historical_chain(path: &Path) -> (ObjectId, ObjectId) {
    let store = SqliteStore::open(path).unwrap();
    let first = store.control_diagnostics().unwrap().active_policy;
    let mut policy: ControlPolicy = store.get(&first).unwrap().unwrap();
    let mut authority: ProjectPolicyAuthorityDecision =
        store.get(&policy.authority).unwrap().unwrap();
    policy.required_assurance = Some(ControlAssurance::TurnGated);
    policy.supported_effects = Some(vec![
        EffectClass::Observe,
        EffectClass::Communicate,
        EffectClass::Coordinate,
        EffectClass::MutateLocal,
    ]);
    policy.grant_ttl_seconds = Some(crate::storage::BUILTIN_CONTROL_GRANT_TTL_SECONDS);
    authority.operation = ProjectPolicyOperation::SetRequiredAssurance;
    authority.required_assurance = policy.required_assurance;
    // Passive historical bytes keep their minted ids and links; no removed
    // operation is invoked to construct this fixture.
    let old_authority = CanonicalObject::identified(&policy.authority, &authority).unwrap();
    store
        .connection
        .execute(
            "UPDATE objects SET canonical_json = ?1 WHERE object_id = ?2",
            rusqlite::params![old_authority.bytes(), policy.authority.as_str()],
        )
        .unwrap();
    replace_policy(&store, &first, &policy);
    authority.policy_epoch = ProjectPolicyEpoch(2);
    authority.previous_policy = Some(first.clone());
    authority.required_assurance = Some(ControlAssurance::Advisory);
    authority.decided_at += TimeDelta::seconds(1);
    authority.authorized_by = actor("historical-policy-operator");
    let next_authority = CanonicalObject::mint(&authority).unwrap();
    SqliteStore::insert_object(
        &store.connection,
        "project_policy_authority_decision",
        &next_authority,
    )
    .unwrap();
    policy.policy_epoch = authority.policy_epoch;
    policy.previous_policy = authority.previous_policy.clone();
    policy.required_assurance = authority.required_assurance;
    policy.authority = next_authority.key().clone();
    policy.activated_at = authority.decided_at;
    let next = CanonicalObject::mint(&policy).unwrap();
    SqliteStore::insert_object(&store.connection, "control_policy", &next).unwrap();
    store.connection.execute("INSERT INTO control_policy_versions (policy_id, policy_epoch, authority_id, policy_json) VALUES (?1, 2, ?2, ?3)",
        rusqlite::params![next.key().as_str(), policy.authority.as_str(), next.bytes()]).unwrap();
    store
        .connection
        .execute(
            "UPDATE control_policy_state SET policy_epoch = 2, policy_id = ?1",
            [next.key().as_str()],
        )
        .unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
    (first, next.key().clone())
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "historical two-version bytes cross migration and both surviving successor operations"
)]
fn historical_admission_policy_chain_imports_unchanged_and_current_successors_omit_metadata() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("historical.db");
    let file = directory.path().join("historical.jsonl");
    let (first, second) = historical_chain(&source);
    let expected = rows(&source);
    export_json(&source, &file).unwrap();
    for acceptance in [false, true] {
        let target = directory.path().join(format!("current-{acceptance}.db"));
        import_json(&file, &target).unwrap();
        assert_eq!(rows(&target), expected);
        let mut store = SqliteStore::open(&target).unwrap();
        assert!(store.verify_all().unwrap().is_healthy());
        let diagnostics = store.control_diagnostics().unwrap();
        assert_eq!(diagnostics.active_policy, second);
        let previous: ControlPolicy = store.get(&second).unwrap().unwrap();
        assert_eq!(previous.previous_policy.as_ref(), Some(&first));
        let historical_objects = rows(&target)["objects"].clone();
        let changed = if acceptance {
            store
                .set_acceptance_evaluation_policy(
                    &AcceptanceEvaluationPolicy {
                        allowed_modes: vec![AcceptanceEvaluationMode::IndependentSession],
                        mechanical_basis: MechanicalBasis::Observed,
                        require_source_freshness: true,
                    },
                    &actor("current-policy-operator"),
                    "current-acceptance",
                    Some(&second),
                    previous.activated_at + TimeDelta::seconds(1),
                    &DevelopmentNoopRedactor,
                )
                .unwrap()
                .active_policy
        } else {
            store
                .set_obligation_rule_set(
                    &ObligationRuleSet {
                        schema_version: crate::OBLIGATION_RULE_SET_SCHEMA_VERSION,
                        rules: vec![],
                    },
                    &actor("current-policy-operator"),
                    "current-rules",
                    Some(&second),
                    previous.activated_at + TimeDelta::seconds(1),
                    &DevelopmentNoopRedactor,
                )
                .unwrap()
                .active_policy
        };
        let successor: ControlPolicy = store.get(&changed).unwrap().unwrap();
        assert_eq!(successor.previous_policy.as_ref(), Some(&second));
        assert_eq!(successor.policy_epoch, ProjectPolicyEpoch(3));
        assert!(successor.required_assurance.is_none());
        assert!(successor.supported_effects.is_none());
        assert!(successor.grant_ttl_seconds.is_none());
        let authority: ProjectPolicyAuthorityDecision =
            store.get(&successor.authority).unwrap().unwrap();
        assert!(authority.required_assurance.is_none());
        let after = rows(&target);
        for original in &historical_objects {
            assert!(after["objects"].contains(original));
        }
        for original in &expected["control_policy_versions"] {
            assert!(after["control_policy_versions"].contains(original));
        }
        assert!(store.verify_all().unwrap().is_healthy());
        drop(store);
        assert!(
            SqliteStore::open(&target)
                .unwrap()
                .verify_all()
                .unwrap()
                .is_healthy()
        );
    }
    // A historical selection still has to preserve the old envelope. Both
    // corruptions keep valid shapes and ids, so validation must check the chain.
    for ttl in [false, true] {
        let connection = Connection::open(&source).unwrap();
        let original: Vec<u8> = connection
            .query_row(
                "SELECT canonical_json FROM objects WHERE object_id = ?1",
                [second.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        let mut policy: ControlPolicy = serde_json::from_slice(&original).unwrap();
        if ttl {
            policy.grant_ttl_seconds = Some(31);
        } else {
            policy.supported_effects = Some(vec![EffectClass::Observe]);
        }
        let corrupt = crate::canonical::canonical_bytes(&policy).unwrap();
        connection
            .execute(
                "UPDATE objects SET canonical_json = ?1 WHERE object_id = ?2",
                rusqlite::params![corrupt, second.as_str()],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE control_policy_versions SET policy_json = ?1 WHERE policy_id = ?2",
                rusqlite::params![corrupt, second.as_str()],
            )
            .unwrap();
        let broken = directory.path().join(format!("broken-{ttl}.jsonl"));
        let target = directory.path().join(format!("broken-{ttl}.db"));
        export_json(&source, &broken).unwrap();
        let error = import_json(&broken, &target).unwrap_err().to_string();
        assert!(
            error.contains(
                "SetRequiredAssurance policy transition changed a preserved policy field"
            ),
            "{error}"
        );
        assert!(!target.exists());
        connection
            .execute(
                "UPDATE objects SET canonical_json = ?1 WHERE object_id = ?2",
                rusqlite::params![original, second.as_str()],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE control_policy_versions SET policy_json = ?1 WHERE policy_id = ?2",
                rusqlite::params![original, second.as_str()],
            )
            .unwrap();
    }
}
