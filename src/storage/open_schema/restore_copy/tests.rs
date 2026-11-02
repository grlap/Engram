use super::*;
use crate::{
    ProjectId, SessionId,
    domain::{ActorContext, AssuranceLevel, ClaimWorkRequest, CreateWorkRequest},
    memory::DevelopmentNoopRedactor,
};
use chrono::{TimeDelta, TimeZone};

#[test]
fn the_live_authority_of_a_copy_counts_only_unexpired_work_claims() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("engram.db");
    let start = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open(&database).unwrap();
    let actor = ActorContext {
        actor_id: "restore-author".into(),
        actor_kind: "test_agent".into(),
        assurance: AssuranceLevel::Asserted,
        run_id: None,
        session_id: Some(SessionId("restore-author".into())),
        source_tool: None,
        source_skill: None,
        provenance_chain: Vec::new(),
        reason: "restore fixture".into(),
    };
    let mut expiries = Vec::new();
    for (index, ttl_seconds) in [10, 60].into_iter().enumerate() {
        let item = store
            .create_work(
                &CreateWorkRequest {
                    project_id: ProjectId("restore-project".into()),
                    parent_id: None,
                    child_requirement: crate::domain::ChildRequirement::Required,
                    title: format!("Restore work {index}"),
                    outcome: "Preserve claim".into(),
                    acceptance: vec!["Claim retained".into()],
                    acceptance_bindings: Vec::new(),
                    evaluation_mode: None,
                    kind: crate::domain::WorkItemKind::Task,
                    priority: 1,
                    labels: Vec::new(),
                    assigned_to: None,
                    deferred_until: None,
                    external_ref: None,
                    notes: Vec::new(),
                    origin: crate::domain::WorkOrigin::Local,
                    source_snapshot_id: None,
                    actor: actor.clone(),
                    idempotency_key: format!("create-{index}"),
                    created_at: start,
                },
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        let claim = store
            .claim_work(
                &ClaimWorkRequest {
                    work_id: item.work_id,
                    expected_work_revision: item.revision,
                    expected_run_id: item.active_run_id,
                    holder: actor.session_id.clone().unwrap(),
                    ttl_seconds,
                    recovery_reason: None,
                    actor: actor.clone(),
                    idempotency_key: format!("claim-{index}"),
                    claimed_at: start,
                },
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        expiries.push(claim.expires_at);
    }
    drop(store);
    for (as_of, count) in [
        (start, 2),
        (start + TimeDelta::seconds(11), 1),
        (expiries[1], 0),
    ] {
        let report = SqliteStore::verify_restore_copy(&database, as_of).unwrap();
        assert_eq!(report.authority.unexpired_claims, count);
        assert_eq!(
            report.authority.claims_expire_by,
            (count > 0).then_some(expiries[1])
        );
        assert_eq!(report.project_ids, ["restore-project"]);
    }
    for sidecar in store_sidecars(&database) {
        assert!(!sidecar.exists(), "{}", sidecar.display());
    }
}
