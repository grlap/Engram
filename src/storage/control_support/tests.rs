use chrono::{TimeDelta, TimeZone};

use super::*;
use crate::storage::{
    control_policy_version_load_count, reset_control_policy_version_load_count, test_support::*,
};
use crate::*;

use crate::{
    DevelopmentNoopRedactor,
    domain::{
        ControlAssurance, EffectClass, ProjectId, ProjectPolicyEpoch, TurnIntent, TurnPurpose,
    },
};

#[test]
fn live_control_policy_load_is_bounded_independently_of_history_depth() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open_in_memory().expect("store");
    let mut active = store
        .control_diagnostics()
        .expect("initial diagnostics")
        .active_policy;
    for epoch in 2_i64..=51 {
        let required_assurance = if epoch % 2 == 0 {
            ControlAssurance::Advisory
        } else {
            ControlAssurance::TurnGated
        };
        let receipt = store
            .set_required_control_assurance(
                required_assurance,
                &actor("policy-load-admin"),
                &format!("install policy epoch {epoch}"),
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
    let binding = bind_control_for(
        &mut store,
        "bounded-policy-session",
        "bind-bounded-policy",
        &[EffectClass::Observe],
        now + TimeDelta::seconds(1),
    );
    assert_eq!(control_policy_version_load_count(), 1);

    reset_control_policy_version_load_count();
    let decision = store
        .evaluate_control_turn(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &TurnIntent {
                idempotency_key: "bounded-policy-turn".into(),
                intent_fingerprint: ObjectHash::from_canonical_bytes(b"bounded-policy-turn"),
                purpose: TurnPurpose::Ordinary,
                requested_effects: vec![EffectClass::Observe],
                resource_intents: Vec::new(),
            },
            now + TimeDelta::seconds(2),
        )
        .expect("evaluate through bounded policy loader");
    assert_eq!(control_policy_version_load_count(), 1);
    let ControlTurnDecision::Grant { grant } = decision else {
        panic!("bounded policy fixture must grant");
    };
    let delivery_tokens = grant
        .delivery
        .iter()
        .map(|delivery| delivery.page.delivery_token.clone())
        .collect::<Vec<_>>();

    reset_control_policy_version_load_count();
    assert!(matches!(
        store
            .begin_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &grant.grant_id,
                &delivery_tokens,
                "begin-bounded-policy-turn",
                now + TimeDelta::seconds(3),
            )
            .expect("begin through bounded policy loader"),
        ControlTurnBeginDecision::Begin { .. }
    ));
    assert_eq!(control_policy_version_load_count(), 1);
}
