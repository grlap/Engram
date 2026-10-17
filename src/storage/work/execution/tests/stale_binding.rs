use super::*;
use crate::domain::{DirectiveSatisfaction, DirectiveTarget};

// Engram owns refusal and explicit authority recovery. Retaining and delivering
// the interrupted prompt is the host's responsibility, outside this fixture.
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "preserve the combined restart, stale binding and explicit recovery sequence"
)]
fn restarted_host_recovers_expired_work_binding_with_fresh_admission() {
    let directory = crate::test_support::temp_home().expect("temporary directory");
    let database = directory.path().join("engram.sqlite3");
    let mut store = SqliteStore::open(&database).expect("store");
    let work = store
        .create_work(
            &root_request("project-restart", "restart-work", 1),
            &DevelopmentNoopRedactor,
        )
        .expect("work");
    let initial_claim = claim(&mut store, &work, "runner", "initial-claim", 2, 10);
    let run = load_work_run(&store.connection, initial_claim.run_id).expect("run");
    let original = ControlWorkBinding {
        root_execution_id: run.root_execution_id,
        work_id: work.work_id,
        run_id: run.run_id,
        work_revision: initial_claim.accepted_work_revision,
        claim_id: initial_claim.claim_id,
        claim_fence: initial_claim.fence,
    };
    let session_id = SessionId("runner".into());
    let mut host_actor = actor("runner");
    host_actor.run_id = Some(run.run_id.0.to_string());
    let connection = store
        .resume_control_connection(&session_id, at(3))
        .expect("initial connection");
    let bound = store
        .bind_control_session_with_work(
            &work.project_id,
            "local-work:restart",
            "Recover stale work binding",
            &session_id,
            &connection,
            &host_actor,
            Some(&original),
            ControlAssurance::TurnGated,
            &[EffectClass::Observe],
            1,
            "initial-bind",
            at(3),
        )
        .expect("initial binding");
    let intent = |key: &str| TurnIntent {
        idempotency_key: key.into(),
        intent_fingerprint: ObjectId::from_canonical_bytes(key.as_bytes()),
        purpose: Some(TurnPurpose::Ordinary),
        requested_effects: vec![EffectClass::Observe],
        resource_intents: Vec::new(),
    };
    let ControlTurnDecision::Grant { grant: retired } = store
        .evaluate_control_turn(
            &work.project_id,
            &session_id,
            &connection,
            &bound.routing_token,
            &intent("before-restart"),
            at(4),
        )
        .expect("initial admission")
    else {
        panic!("live work must grant before restart");
    };
    drop(store);

    // Expiry makes the binding stale; reopening alone does not expire claims.
    assert_eq!(initial_claim.expires_at, at(12));
    let mut store = SqliteStore::open(&database).expect("reopened store");
    let connection = store
        .resume_control_connection(&session_id, at(13))
        .expect("replacement host connection");
    let status = store
        .control_status(
            &work.project_id,
            &session_id,
            &connection,
            &bound.routing_token,
            at(13),
        )
        .expect("retained binding");
    assert_eq!(status.work_binding.as_ref(), Some(&original));
    assert_eq!(
        store
            .begin_control_turn(
                &work.project_id,
                &session_id,
                &connection,
                &bound.routing_token,
                &retired.grant_id,
                &[],
                "retired-begin",
                at(14),
            )
            .expect("retired grant refusal"),
        ControlTurnBeginDecision::Refuse {
            code: ControlRefusalCode::GrantScopeMismatch,
        }
    );
    let stale_intent = intent("stale-binding");
    let stale = store
        .evaluate_control_turn(
            &work.project_id,
            &session_id,
            &connection,
            &bound.routing_token,
            &stale_intent,
            at(15),
        )
        .expect("stale binding is a typed refusal");
    let ControlTurnDecision::Refuse { directive } = &stale else {
        panic!("expired claim must refuse: {stale:?}");
    };
    assert_eq!(directive.code, ControlRefusalCode::StaleFence);
    assert_eq!(directive.target, DirectiveTarget::Host);
    assert_eq!(
        directive.satisfaction,
        DirectiveSatisfaction::HostTransition
    );
    assert_eq!(directive.recovery_effects, vec![EffectClass::Observe]);
    let error = store
        .bind_control_session_with_work(
            &work.project_id,
            "local-work:restart",
            "Recover stale work binding",
            &session_id,
            &connection,
            &host_actor,
            Some(&original),
            ControlAssurance::TurnGated,
            &[EffectClass::Observe],
            1,
            "stale-bind",
            at(16),
        )
        .expect_err("the old tuple cannot silently recover a claim");
    assert!(matches!(error, StoreError::ControlWorkBindingStale { .. }));
    assert!(
        error
            .to_string()
            .contains("reread the live claim and rebind")
    );
    assert_eq!(
        store.current_work_claim(work.work_id).expect("claim"),
        Some(initial_claim.clone()),
        "connection recovery, reads and refusals must not renew the claim"
    );

    let recovered = claim(&mut store, &work, "runner", "recover-claim", 17, 120);
    assert!(recovered.fence > initial_claim.fence);
    let replacement = ControlWorkBinding {
        work_revision: recovered.accepted_work_revision,
        claim_id: recovered.claim_id,
        claim_fence: recovered.fence,
        ..original
    };
    let rebound = store
        .bind_control_session_with_work(
            &work.project_id,
            "local-work:restart",
            "Recover stale work binding",
            &session_id,
            &connection,
            &host_actor,
            Some(&replacement),
            ControlAssurance::TurnGated,
            &[EffectClass::Observe],
            1,
            "recovered-bind",
            at(18),
        )
        .expect("explicitly bind the recovered claim");
    assert_eq!(rebound.status.work_binding.as_ref(), Some(&replacement));
    assert_eq!(
        store
            .evaluate_control_turn(
                &work.project_id,
                &session_id,
                &connection,
                &rebound.routing_token,
                &stale_intent,
                at(19),
            )
            .expect("exact retry retains the original refusal"),
        stale
    );
    assert_eq!(
        store
            .begin_control_turn(
                &work.project_id,
                &session_id,
                &connection,
                &rebound.routing_token,
                &retired.grant_id,
                &[],
                "retired-begin-after-recovery",
                at(20),
            )
            .expect("recovery cannot resurrect retired authority"),
        ControlTurnBeginDecision::Refuse {
            code: ControlRefusalCode::GrantScopeMismatch,
        }
    );
    let ControlTurnDecision::Grant { grant } = store
        .evaluate_control_turn(
            &work.project_id,
            &session_id,
            &connection,
            &rebound.routing_token,
            &intent("recovered-admission"),
            at(21),
        )
        .expect("fresh admission")
    else {
        panic!("recovered binding must grant a fresh intent");
    };
    assert_ne!(grant.grant_id, retired.grant_id);
    assert_eq!(grant.basis.work_binding.as_ref(), Some(&replacement));
    assert!(matches!(
        store
            .begin_control_turn(
                &work.project_id,
                &session_id,
                &connection,
                &rebound.routing_token,
                &grant.grant_id,
                &[],
                "recovered-begin",
                at(22),
            )
            .expect("fresh grant begins"),
        ControlTurnBeginDecision::Begin { .. }
    ));
}
