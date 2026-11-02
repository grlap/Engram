use super::*;
use crate::domain::{
    AcceptanceEvaluationMode, AcceptanceEvaluationPolicy, MechanicalBasis, ProvenanceLink,
    ProvenanceRelation,
};
use serde_json::Value;

#[derive(Clone, Copy, Debug)]
enum Selection {
    EmptyRules,
    StockRules,
    ObservedAcceptance,
    SelfAssertedAcceptance,
}

impl Selection {
    const CHANGES: [Self; 2] = [Self::EmptyRules, Self::ObservedAcceptance];

    fn operation(self) -> &'static str {
        match self {
            Self::EmptyRules | Self::StockRules => "set_obligation_rule_set",
            Self::ObservedAcceptance | Self::SelfAssertedAcceptance => "set_acceptance_evaluation",
        }
    }

    fn other_payload(self) -> Self {
        match self {
            Self::EmptyRules => Self::StockRules,
            Self::ObservedAcceptance => Self::SelfAssertedAcceptance,
            _ => unreachable!("only changed selections request their old payload"),
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "share one assertion matrix across both existing policy setter signatures"
    )]
    fn apply<R: Redactor>(
        self,
        store: &mut SqliteStore,
        administrator: &ActorContext,
        key: &str,
        expected: Option<&ObjectId>,
        now: DateTime<Utc>,
        redactor: &R,
    ) -> Result<Value, StoreError> {
        match self {
            Self::EmptyRules | Self::StockRules => {
                let rules = if matches!(self, Self::StockRules) {
                    crate::control::builtin_obligation_rule_set()
                } else {
                    ObligationRuleSet {
                        schema_version: OBLIGATION_RULE_SET_SCHEMA_VERSION,
                        rules: vec![],
                    }
                };
                Ok(serde_json::to_value(store.set_obligation_rule_set(
                    &rules,
                    administrator,
                    key,
                    expected,
                    now,
                    redactor,
                )?)?)
            }
            Self::ObservedAcceptance | Self::SelfAssertedAcceptance => {
                let policy = if matches!(self, Self::SelfAssertedAcceptance) {
                    AcceptanceEvaluationPolicy::default()
                } else {
                    AcceptanceEvaluationPolicy {
                        allowed_modes: vec![AcceptanceEvaluationMode::IndependentSession],
                        mechanical_basis: MechanicalBasis::Observed,
                        require_source_freshness: true,
                    }
                };
                Ok(serde_json::to_value(
                    store.set_acceptance_evaluation_policy(
                        &policy,
                        administrator,
                        key,
                        expected,
                        now,
                        redactor,
                    )?,
                )?)
            }
        }
    }
}

fn receipt_id(receipt: &Value, field: &str) -> ObjectId {
    ObjectId::from_stored(receipt[field].as_str().unwrap().to_owned()).unwrap()
}

#[test]
fn both_policy_setters_keep_canonical_receipts_and_refuse_stale_cas_without_writes() {
    for selection in Selection::CHANGES {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let initial = store.control_diagnostics().unwrap();
        let receipt = selection
            .apply(
                &mut store,
                &actor("admin"),
                "change",
                Some(&initial.active_policy),
                Utc::now(),
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        let current = receipt_id(&receipt, "active_policy");
        assert_eq!(receipt["changed"], true);
        assert_eq!(receipt["policy_epoch"], 2);
        assert_eq!(
            receipt["previous_policy"],
            serde_json::to_value(&initial.active_policy).unwrap()
        );
        assert_projection_bytes(
            &store,
            "SELECT result_json FROM control_policy_operation_results WHERE operation=?1 AND idempotency_key=?2",
            params![selection.operation(), "change"],
            &receipt,
        );
        let before = test_database_shape_snapshot(&store.connection).unwrap();
        let refused = selection
            .apply(
                &mut store,
                &actor("admin"),
                "stale",
                Some(&initial.active_policy),
                Utc::now(),
                &DevelopmentNoopRedactor,
            )
            .unwrap_err();
        assert!(
            matches!(refused, StoreError::ControlPolicyConflict { expected, current: actual }
            if expected == initial.active_policy && actual == current)
        );
        assert_eq!(
            test_database_shape_snapshot(&store.connection).unwrap(),
            before
        );
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one receipt history covers restart, changed and no-op replay, conflicts and doctor corruption"
)]
fn policy_receipts_replay_after_restart_and_later_heads_and_doctor_names_corruption() {
    for selection in Selection::CHANGES {
        let home = crate::test_support::temp_home().unwrap();
        let database = home.path().join("policy.db");
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let mut store = SqliteStore::open(&database).unwrap();
        let initial = store.control_diagnostics().unwrap();
        let administrator = actor("admin");
        let changed = selection
            .apply(
                &mut store,
                &administrator,
                "change",
                Some(&initial.active_policy),
                now,
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        let changed_id = receipt_id(&changed, "active_policy");
        let noop = selection
            .apply(
                &mut store,
                &administrator,
                "noop",
                Some(&changed_id),
                now + TimeDelta::seconds(1),
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        assert_eq!(noop["changed"], false);
        drop(store);
        let mut store = SqliteStore::open(&database).unwrap();
        selection
            .other_payload()
            .apply(
                &mut store,
                &administrator,
                "later",
                Some(&changed_id),
                now + TimeDelta::seconds(2),
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        for (key, expected, receipt) in [
            ("change", &initial.active_policy, &changed),
            ("noop", &changed_id, &noop),
        ] {
            let before = test_database_shape_snapshot(&store.connection).unwrap();
            let replay = selection
                .apply(
                    &mut store,
                    &administrator,
                    key,
                    Some(expected),
                    now + TimeDelta::seconds(10),
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
            assert_eq!(&replay, receipt);
            assert_eq!(
                test_database_shape_snapshot(&store.connection).unwrap(),
                before
            );
        }
        let before_conflicts = test_database_shape_snapshot(&store.connection).unwrap();
        for conflict in [
            selection.other_payload().apply(
                &mut store,
                &administrator,
                "change",
                Some(&initial.active_policy),
                now,
                &DevelopmentNoopRedactor,
            ),
            selection.apply(
                &mut store,
                &actor("another-admin"),
                "change",
                Some(&initial.active_policy),
                now,
                &DevelopmentNoopRedactor,
            ),
            selection.apply(
                &mut store,
                &administrator,
                "change",
                None,
                now,
                &DevelopmentNoopRedactor,
            ),
        ] {
            assert!(
                matches!(conflict, Err(StoreError::ControlOperationIdempotencyConflict { operation, key })
                if operation == selection.operation() && key == "change")
            );
        }
        assert_eq!(
            test_database_shape_snapshot(&store.connection).unwrap(),
            before_conflicts
        );
        assert!(store.verify_all().unwrap().is_healthy());
        let sequence: i64 = store.connection.query_row(
            "SELECT sequence FROM control_policy_operation_results WHERE operation=?1 AND idempotency_key='change'",
            [selection.operation()], |row| row.get(0),
        ).unwrap();
        store
            .connection
            .execute(
                "UPDATE control_policy_operation_results SET result_json=?1 WHERE sequence=?2",
                params![
                    crate::canonical::canonical_bytes(&serde_json::json!({})).unwrap(),
                    sequence
                ],
            )
            .unwrap();
        let report = store.verify_all().unwrap();
        assert!(
            report
                .invalid_control_records
                .contains(&format!("control_policy_operation:{sequence}")),
            "{report:?}"
        );
    }
}

#[test]
fn a_rolled_back_policy_selector_cannot_hide_a_later_history_head() {
    for selection in Selection::CHANGES {
        let home = crate::test_support::temp_home().unwrap();
        let database = home.path().join("policy.db");
        let mut store = SqliteStore::open(&database).unwrap();
        let initial = store.control_diagnostics().unwrap();
        let changed = selection
            .apply(
                &mut store,
                &actor("admin"),
                "change",
                Some(&initial.active_policy),
                Utc::now(),
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE control_policy_state SET policy_epoch=?1, policy_id=?2 WHERE singleton=1",
                params![initial.policy_epoch.0, initial.active_policy.as_str()],
            )
            .unwrap();
        assert!(
            matches!(store.control_diagnostics(), Err(StoreError::InvalidControlProjection(reason)) if reason.contains("maximal history head"))
        );
        let report = store.verify_all().unwrap();
        assert!(
            report
                .invalid_control_records
                .contains(&"control_policy_state:active".into())
        );
        assert!(report.invalid_control_records.contains(&format!(
            "control_policy_version:{}",
            receipt_id(&changed, "active_policy")
        )));
        drop(store);
        assert!(
            matches!(SqliteStore::open(&database), Err(StoreError::InvalidControlProjection(reason)) if reason.contains("maximal history head"))
        );
    }
}

fn attribution_field(actor: &mut ActorContext, field: &str, value: String) {
    match field {
        "actor" => actor.actor_id = value,
        "kind" => actor.actor_kind = value,
        "reason" => actor.reason = value,
        "run" => actor.run_id = Some(value),
        "session" => actor.session_id = Some(SessionId(value)),
        "tool" => actor.source_tool = Some(value),
        "skill" => actor.source_skill = Some(value),
        "source" => {
            actor.provenance_chain = vec![ProvenanceLink {
                relation: ProvenanceRelation::AssertedBy,
                source: value,
                reference: None,
            }];
        }
        "reference" => {
            actor.provenance_chain = vec![ProvenanceLink {
                relation: ProvenanceRelation::AssertedBy,
                source: "source".into(),
                reference: Some(value),
            }];
        }
        _ => unreachable!("enumerated attribution field"),
    }
}

#[test]
fn every_policy_attribution_leaf_is_redacted_and_refusal_has_no_effects_or_prose_leak() {
    for selection in Selection::CHANGES {
        let mut store = SqliteStore::open_in_memory().unwrap();
        for field in [
            "actor",
            "kind",
            "reason",
            "run",
            "session",
            "tool",
            "skill",
            "source",
            "reference",
        ] {
            let mut administrator = actor("admin");
            attribution_field(&mut administrator, field, "reject-me".into());
            let before = test_database_shape_snapshot(&store.connection).unwrap();
            let error = selection
                .apply(
                    &mut store,
                    &administrator,
                    field,
                    None,
                    Utc::now(),
                    &SentinelRedactor,
                )
                .unwrap_err();
            assert!(
                matches!(error, StoreError::RedactionRefused(_)),
                "{field}: {error}"
            );
            assert_eq!(crate::host::store_error_code(&error), "redaction_refused");
            assert!(!error.to_string().contains("reject-me"));
            assert_eq!(
                test_database_shape_snapshot(&store.connection).unwrap(),
                before
            );
        }
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "both setters share the same attribution boundary and normalization matrix"
)]
fn policy_attribution_bounds_apply_before_writes_and_normalized_leaves_are_retained() {
    for selection in Selection::CHANGES {
        let mut store = SqliteStore::open_in_memory().unwrap();
        for field in [
            "actor",
            "kind",
            "reason",
            "run",
            "tool",
            "skill",
            "source",
            "reference",
        ] {
            let mut administrator = actor("admin");
            attribution_field(&mut administrator, field, "x".repeat(4097));
            let before = test_database_shape_snapshot(&store.connection).unwrap();
            assert!(
                matches!(selection.apply(&mut store, &administrator, "oversized", None, Utc::now(), &DevelopmentNoopRedactor), Err(StoreError::InvalidControlProjection(reason)) if reason.contains("1 through 4096 bytes"))
            );
            assert_eq!(
                test_database_shape_snapshot(&store.connection).unwrap(),
                before
            );
        }
        for count in [
            crate::storage::MAX_CONTROL_POLICY_PROVENANCE_LINKS + 1,
            crate::storage::MAX_CONTROL_POLICY_PROVENANCE_LINKS,
        ] {
            let mut administrator = actor("admin");
            administrator.provenance_chain = (0..count)
                .map(|_| ProvenanceLink {
                    relation: ProvenanceRelation::RelayedBy,
                    source: "s".repeat(2000),
                    reference: Some("r".repeat(2000)),
                })
                .collect();
            let before = test_database_shape_snapshot(&store.connection).unwrap();
            let error = selection
                .apply(
                    &mut store,
                    &administrator,
                    "oversized-provenance",
                    None,
                    Utc::now(),
                    &DevelopmentNoopRedactor,
                )
                .unwrap_err();
            assert!(matches!(error, StoreError::InvalidControlProjection(_)));
            assert!(error.to_string().contains(
                if count > crate::storage::MAX_CONTROL_POLICY_PROVENANCE_LINKS {
                    "provenance must contain at most"
                } else {
                    "attribution exceeds"
                }
            ));
            assert_eq!(
                test_database_shape_snapshot(&store.connection).unwrap(),
                before
            );
        }
        for session in [ascii65_session(), utf8_oversized_session()] {
            let mut administrator = actor("admin");
            administrator.session_id = Some(session.clone());
            let before = test_database_shape_snapshot(&store.connection).unwrap();
            let error = selection
                .apply(
                    &mut store,
                    &administrator,
                    "oversized-session",
                    None,
                    Utc::now(),
                    &DevelopmentNoopRedactor,
                )
                .unwrap_err();
            assert!(
                matches!(&error, StoreError::InvalidWork(reason) if reason == crate::domain::SessionIdAdmissionError::TooLong.as_str())
            );
            assert!(!error.to_string().contains(&session.0));
            assert_eq!(
                test_database_shape_snapshot(&store.connection).unwrap(),
                before
            );
        }
        for session in [exact_64_ascii_session(), exact_64_utf8_session()] {
            let mut session_store = SqliteStore::open_in_memory().unwrap();
            let mut administrator = actor(" admin ");
            administrator.actor_kind = " agent ".into();
            administrator.reason = " policy update ".into();
            administrator.run_id = Some(" run ".into());
            administrator.session_id = Some(SessionId(session.clone()));
            administrator.source_tool = Some(" tool ".into());
            administrator.source_skill = Some(" skill ".into());
            administrator.provenance_chain = vec![ProvenanceLink {
                relation: ProvenanceRelation::AssertedBy,
                source: " source ".into(),
                reference: Some(" reference ".into()),
            }];
            let receipt = selection
                .apply(
                    &mut session_store,
                    &administrator,
                    &session,
                    None,
                    Utc::now(),
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
            let authority: ProjectPolicyAuthorityDecision = session_store
                .get(&receipt_id(&receipt, "authority"))
                .unwrap()
                .unwrap();
            let saved = authority.authorized_by;
            assert_eq!(saved.actor_id, "admin");
            assert_eq!(saved.actor_kind, "agent");
            assert_eq!(saved.reason, "policy update");
            assert_eq!(saved.run_id.as_deref(), Some("run"));
            assert_eq!(saved.session_id, Some(SessionId(session)));
            assert_eq!(saved.source_tool.as_deref(), Some("tool"));
            assert_eq!(saved.source_skill.as_deref(), Some("skill"));
            assert_eq!(saved.provenance_chain[0].source, "source");
            assert_eq!(
                saved.provenance_chain[0].reference.as_deref(),
                Some("reference")
            );
            assert!(session_store.verify_all().unwrap().is_healthy());
        }
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

#[test]
fn historical_authority_extra_members_survive_current_policy_updates() {
    for selection in Selection::CHANGES {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let initial = store.control_diagnostics().unwrap();
        let policy: ControlPolicy = store.get(&initial.active_policy).unwrap().unwrap();
        let mut historical = serde_json::to_value(
            store
                .get::<ProjectPolicyAuthorityDecision>(&policy.authority)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        historical["reason"] = "retained historical justification".into();
        let bytes = crate::canonical::canonical_bytes(&historical).unwrap();
        store
            .connection
            .execute(
                "UPDATE objects SET canonical_json=?1 WHERE object_id=?2",
                params![bytes, policy.authority.as_str()],
            )
            .unwrap();
        let receipt = selection
            .apply(
                &mut store,
                &actor("admin"),
                "current",
                Some(&initial.active_policy),
                Utc::now(),
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        let retained: Vec<u8> = store
            .connection
            .query_row(
                "SELECT canonical_json FROM objects WHERE object_id=?1",
                [policy.authority.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained, bytes);
        let authority: Value = store
            .get(&receipt_id(&receipt, "authority"))
            .unwrap()
            .unwrap();
        assert!(authority.get("reason").is_none());
        assert!(authority["authorized_by"].get("reason").is_some());
        let intent: Vec<u8> = store.connection.query_row("SELECT intent_json FROM control_policy_operation_results WHERE operation=?1 AND idempotency_key='current'", [selection.operation()], |row| row.get(0)).unwrap();
        assert!(
            serde_json::from_slice::<Value>(&intent)
                .unwrap()
                .get("reason")
                .is_none()
        );
        assert!(store.verify_all().unwrap().is_healthy());
    }
}
