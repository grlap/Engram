use chrono::{TimeDelta, TimeZone};

use super::*;
use crate::{
    ObjectId, ProjectId,
    domain::{ControlTurnBeginDecision, ControlTurnDecision, EffectClass, TurnIntent, TurnPurpose},
    storage::test_support::{TestControlBinding, bind_control_for},
};

/// Evaluates one turn for `binding` at `at` and returns its grant id.
fn issue(
    store: &mut SqliteStore,
    binding: &TestControlBinding,
    key: &str,
    at: DateTime<Utc>,
) -> String {
    let decision = store
        .evaluate_control_turn(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &TurnIntent {
                idempotency_key: key.into(),
                intent_fingerprint: ObjectId::from_canonical_bytes(key.as_bytes()),
                purpose: Some(TurnPurpose::Ordinary),
                requested_effects: vec![EffectClass::Observe],
                resource_intents: Vec::new(),
            },
            at,
        )
        .unwrap();
    let ControlTurnDecision::Grant { grant } = decision else {
        panic!("turn {key} must grant");
    };
    grant.grant_id
}

fn expires_at(store: &SqliteStore, grant: &str) -> DateTime<Utc> {
    let ms: i64 = store
        .connection
        .query_row(
            "SELECT expires_at_ms FROM control_turn_grants WHERE grant_id = ?1",
            [grant],
            |row| row.get(0),
        )
        .unwrap();
    DateTime::from_timestamp_millis(ms).unwrap()
}

#[test]
fn the_live_authority_of_a_copy_counts_unexpired_issued_grants_and_begun_turns_apart() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("engram.db");
    let start = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open(&database).unwrap();
    let early = bind_control_for(
        &mut store,
        "early-session",
        "bind-early",
        &[EffectClass::Observe],
        start,
    );
    let late = bind_control_for(
        &mut store,
        "late-session",
        "bind-late",
        &[EffectClass::Observe],
        start,
    );
    let begun = bind_control_for(
        &mut store,
        "begun-session",
        "bind-begun",
        &[EffectClass::Observe],
        start,
    );

    // An issued grant that will have expired, one that will not, and a grant
    // whose turn began.
    let early_grant = issue(
        &mut store,
        &early,
        "early-turn",
        start + TimeDelta::seconds(1),
    );
    let late_grant = issue(
        &mut store,
        &late,
        "late-turn",
        start + TimeDelta::seconds(10),
    );
    let begun_grant = issue(
        &mut store,
        &begun,
        "begun-turn",
        start + TimeDelta::seconds(11),
    );
    assert!(matches!(
        store
            .begin_control_turn(
                &ProjectId("project-a".into()),
                &begun.status.session_id,
                &begun.connection_token,
                &begun.routing_token,
                &begun_grant,
                &[],
                "begin-begun-turn",
                start + TimeDelta::seconds(12),
            )
            .unwrap(),
        ControlTurnBeginDecision::Begin { .. }
    ));
    let early_expiry = expires_at(&store, &early_grant);
    let late_expiry = expires_at(&store, &late_grant);
    assert!(early_expiry < late_expiry);
    drop(store);

    // Between the two expiries, one issued grant is live; the begun turn is
    // counted apart, whatever its grant's expiry.
    let as_of = early_expiry + TimeDelta::seconds(1);
    let report = SqliteStore::verify_restore_copy(&database, as_of).unwrap();
    assert_eq!(
        report.authority,
        LiveAuthority {
            as_of,
            unexpired_claims: 0,
            claims_expire_by: None,
            unexpired_grants: 1,
            grants_expire_by: Some(late_expiry),
            begun_turns: 1,
        }
    );
    assert_eq!(report.project_ids, ["project-a"]);

    // Before both expiries, both issued grants are live and the last named.
    let report =
        SqliteStore::verify_restore_copy(&database, start + TimeDelta::seconds(20)).unwrap();
    assert_eq!(report.authority.unexpired_grants, 2);
    assert_eq!(report.authority.grants_expire_by, Some(late_expiry));
    assert_eq!(report.authority.begun_turns, 1);

    // After both, none is, and the begun turn still counts.
    let report = SqliteStore::verify_restore_copy(&database, late_expiry).unwrap();
    assert_eq!(report.authority.unexpired_grants, 0);
    assert_eq!(report.authority.grants_expire_by, None);
    assert_eq!(report.authority.begun_turns, 1);
    // The check wrote nothing beside the copy.
    for sidecar in store_sidecars(&database) {
        assert!(!sidecar.exists(), "{}", sidecar.display());
    }
}
