use chrono::{TimeDelta, TimeZone};

use super::*;
use crate::storage::test_support::*;
use crate::*;

use crate::{
    DevelopmentNoopRedactor,
    domain::{
        ControlAssurance, EffectClass, NoteVisibility, ProjectId, SessionPhase, TurnIntent,
        TurnPurpose,
    },
};

#[test]
fn environment_components_are_redactor_inspected_before_canonicalization() {
    let components = EnvironmentComponents {
        toolchain: "reject-me-toolchain".into(),
        sandbox: Some("sandbox-v1".into()),
        workspace_id: "workspace-redaction".into(),
        capability_map_revision: 1,
    };
    let input = EnvironmentEvidenceInput {
        source_basis: crate::ExecutionSourceBasis {
            workspace_id: components.workspace_id.clone(),
            source_revision: "revision-redaction".into(),
        },
        environment_fingerprint: environment_components_fingerprint(&components)
            .expect("freeze environment components"),
        components: Some(components),
        observed_at: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
    };
    assert!(matches!(
        validate_typed_evidence_inputs(
            &[],
            &[input],
            Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
            &SentinelRedactor,
        ),
        Err(StoreError::RedactionRefused(_))
    ));
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the single grant lifecycle fixture keeps issued replacement and begun checkpoint recovery adjacent"
)]
fn fresh_evaluate_replaces_issued_grant_but_preserves_begun_checkpoint() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open_in_memory().expect("store");
    let binding = bind_control(&mut store, now);
    complete_control_turn(
        &mut store,
        &binding,
        "initial-sync",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(1),
    );
    let evaluate = |store: &mut SqliteStore, key: &str, at: DateTime<Utc>| {
        store
            .evaluate_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &TurnIntent {
                    idempotency_key: key.into(),
                    intent_fingerprint: ObjectId::from_canonical_bytes(key.as_bytes()),
                    purpose: TurnPurpose::Ordinary,
                    requested_effects: vec![EffectClass::Observe],
                    resource_intents: Vec::new(),
                },
                at,
            )
            .expect("evaluate turn")
    };
    let ControlTurnDecision::Grant { grant: first } = evaluate(
        &mut store,
        "replace-issued-first",
        now + TimeDelta::seconds(2),
    ) else {
        panic!("first grant must issue");
    };
    let status = store
        .control_status(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            now + TimeDelta::seconds(2),
        )
        .expect("issued status");
    assert_eq!(
        status.open_grant_id.as_deref(),
        Some(first.grant_id.as_str())
    );
    assert_eq!(status.open_grant_state, Some(TurnGrantState::Issued));

    let refused = store
        .checkpoint_control_turn(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &first.grant_id,
            TurnNextIntent::Continue,
            "checkpoint-issued",
            now + TimeDelta::seconds(3),
        )
        .expect("issued checkpoint is a refusal");
    assert!(matches!(
        refused,
        ControlTurnCheckpointDecision::Refuse {
            code: ControlRefusalCode::GrantNotBegun,
            directive: Some(crate::domain::ControlDirective {
                target: crate::domain::DirectiveTarget::Host,
                satisfaction: crate::domain::DirectiveSatisfaction::HostTransition,
                ..
            })
        }
    ));

    let ControlTurnDecision::Grant { grant: second } = evaluate(
        &mut store,
        "replace-issued-second",
        now + TimeDelta::seconds(4),
    ) else {
        panic!("fresh evaluation must replace issued grant");
    };
    assert_ne!(first.grant_id, second.grant_id);
    assert_projection_bytes(
        &store,
        "SELECT grant_json FROM control_turn_grants WHERE grant_id = ?1",
        [&second.grant_id],
        &second,
    );
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT state FROM control_turn_grants WHERE grant_id = ?1",
                [&first.grant_id],
                |row| row.get::<_, String>(0),
            )
            .expect("superseded state"),
        "superseded"
    );
    let (supersession_json, replacement_decision_hash) = store
        .connection
        .query_row(
            "SELECT supersession_json, replacement_decision_hash
             FROM control_turn_grant_supersessions
             WHERE superseded_grant_id = ?1",
            [&first.grant_id],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
        )
        .expect("immutable supersession transition");
    let supersession: TurnGrantSupersession =
        SqliteStore::decode_json_projection(&supersession_json)
            .expect("verified supersession transition");
    let replacement = ControlTurnDecision::Grant {
        grant: second.clone(),
    };
    let expected_transition = TurnGrantSupersession {
        control_schema_version: CONTROL_SCHEMA_VERSION,
        session_id: binding.status.session_id.clone(),
        task_id: first.basis.task_id,
        superseded_grant_id: first.grant_id.clone(),
        superseded_request_key: first.request_key.clone(),
        replacement_request_key: second.request_key.clone(),
        replacement_decision: CanonicalObject::freeze(&replacement).unwrap().key().clone(),
        reason: TurnGrantSupersessionReason::FreshEvaluation,
        superseded_at: now + TimeDelta::seconds(4),
    };
    assert_eq!(
        supersession_json,
        CanonicalObject::freeze(&expected_transition)
            .unwrap()
            .bytes()
    );
    assert_eq!(
        replacement_decision_hash,
        CanonicalObject::freeze(&replacement)
            .unwrap()
            .key()
            .as_str()
    );
    assert_eq!(supersession.superseded_grant_id, first.grant_id);
    assert_eq!(supersession.superseded_request_key, first.request_key);
    assert_eq!(supersession.replacement_request_key, second.request_key);
    assert_eq!(
        supersession.replacement_decision.as_str(),
        replacement_decision_hash
    );
    assert_eq!(
        supersession.reason,
        TurnGrantSupersessionReason::FreshEvaluation
    );
    let status = store
        .control_status(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            now + TimeDelta::seconds(4),
        )
        .expect("replacement status");
    assert_eq!(
        status.open_grant_id.as_deref(),
        Some(second.grant_id.as_str())
    );
    assert_eq!(status.open_grant_state, Some(TurnGrantState::Issued));

    assert!(matches!(
        store
            .begin_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &second.grant_id,
                &second
                    .delivery
                    .iter()
                    .map(|delivery| delivery.page.delivery_token.clone())
                    .collect::<Vec<_>>(),
                "begin-replacement",
                now + TimeDelta::seconds(5),
            )
            .expect("begin replacement"),
        ControlTurnBeginDecision::Begin { .. }
    ));
    assert!(matches!(
        evaluate(&mut store, "while-begun", now + TimeDelta::seconds(6)),
        ControlTurnDecision::Refuse { directive }
            if directive.code == ControlRefusalCode::TurnAlreadyOpen
    ));
    let status = store
        .control_status(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            now + TimeDelta::seconds(6),
        )
        .expect("begun status");
    assert_eq!(status.open_grant_state, Some(TurnGrantState::Begun));
    assert!(matches!(
        store
            .checkpoint_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &second.grant_id,
                TurnNextIntent::Continue,
                "checkpoint-replacement",
                now + TimeDelta::seconds(40),
            )
            .expect("begun checkpoint survives grant expiry"),
        ControlTurnCheckpointDecision::Checkpointed { .. }
    ));
    let report = store.verify_all().expect("verified grant supersession");
    assert!(report.is_healthy(), "{report:?}");
    store
        .connection
        .execute(
            "DELETE FROM control_turn_grant_supersessions
             WHERE superseded_grant_id = ?1",
            [&first.grant_id],
        )
        .expect("remove supersession audit fixture");
    let report = store.verify_all().expect("missing supersession report");
    assert!(
        report
            .invalid_control_records
            .contains(&format!("control_turn_grant:{}", first.grant_id))
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one lifecycle test preserves the restart and stale-grant sequence"
)]
fn host_control_turn_is_restart_safe_and_fails_closed_on_drift() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("engram.db");
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open(&database).unwrap();
    let binding = bind_control(&mut store, now);
    assert_eq!(binding.status.phase, SessionPhase::SyncRequired);
    assert_eq!(
        store
            .bind_control_session(
                &ProjectId("project-a".into()),
                "dummy:CONTROL-HOST-1",
                "Exercise the host control lifecycle",
                &binding.status.session_id,
                &binding.connection_token,
                &actor("control-session"),
                ControlAssurance::TurnGated,
                &[EffectClass::Observe, EffectClass::Communicate],
                1,
                "bind-control-a",
                now,
            )
            .unwrap(),
        binding.binding
    );
    assert!(matches!(
        store.control_status(
            &ProjectId("project-a".into()),
            &SessionId("control-session".into()),
            &binding.connection_token,
            "wrong-token",
            now,
        ),
        Err(StoreError::ControlSessionTokenMismatch(_))
    ));
    let private_writer = SessionId("private-writer".into());
    store
        .join_task(
            &ProjectId("project-a".into()),
            "dummy:CONTROL-HOST-1",
            &private_writer,
            actor("private-writer"),
            now,
        )
        .expect("join a concurrent session for the same logical agent");

    let first_intent = TurnIntent {
        idempotency_key: "host-turn-a".into(),
        intent_fingerprint: ObjectId::from_canonical_bytes(b"host-turn-a"),
        purpose: crate::domain::TurnPurpose::Ordinary,
        requested_effects: vec![EffectClass::Observe],
        resource_intents: Vec::new(),
    };
    let first = store
        .evaluate_control_turn(
            &ProjectId("project-a".into()),
            &SessionId("control-session".into()),
            &binding.connection_token,
            &binding.routing_token,
            &first_intent,
            now + TimeDelta::seconds(1),
        )
        .unwrap();
    let crate::domain::ControlTurnDecision::Grant { grant: first_grant } = first else {
        panic!("initial synchronized turn must grant");
    };
    assert!(first_grant.delivery.is_some());
    let mut private_request = note_request(
        binding.status.task_id,
        "private-writer",
        "Constraint: a new owner-private rule invalidates an unbegun turn.",
        "host-private-drift-a",
        NoteVisibility::Private,
    );
    private_request.actor.actor_id = "control-session".into();
    let private_receipt = store
        .capture_note(&private_request, &DevelopmentNoopRedactor)
        .unwrap();
    assert_eq!(private_receipt.cursor, None);
    let stale_begin = store
        .begin_control_turn(
            &ProjectId("project-a".into()),
            &SessionId("control-session".into()),
            &binding.connection_token,
            &binding.routing_token,
            &first_grant.grant_id,
            &[first_grant
                .delivery
                .as_ref()
                .unwrap()
                .page
                .delivery_token
                .clone()],
            "begin-stale-a",
            now + TimeDelta::seconds(2),
        )
        .unwrap();
    assert!(matches!(
        stale_begin,
        ControlTurnBeginDecision::Refuse {
            code: crate::domain::ControlRefusalCode::DeltaRequired
        }
    ));
    drop(store);

    let mut reopened = SqliteStore::open(&database).unwrap();
    let reopened_connection = reopened
        .resume_control_connection(
            &SessionId("control-session".into()),
            now + TimeDelta::seconds(3),
        )
        .unwrap();
    let second_intent = TurnIntent {
        idempotency_key: "host-turn-b".into(),
        intent_fingerprint: ObjectId::from_canonical_bytes(b"host-turn-b"),
        purpose: crate::domain::TurnPurpose::Ordinary,
        requested_effects: vec![EffectClass::Observe, EffectClass::Communicate],
        resource_intents: Vec::new(),
    };
    let second = reopened
        .evaluate_control_turn(
            &ProjectId("project-a".into()),
            &SessionId("control-session".into()),
            &reopened_connection,
            &binding.routing_token,
            &second_intent,
            now + TimeDelta::seconds(3),
        )
        .unwrap();
    let crate::domain::ControlTurnDecision::Grant { grant } = second else {
        panic!("fresh synchronized turn must grant");
    };
    let delivery_token = grant.delivery.as_ref().unwrap().page.delivery_token.clone();
    let begun = reopened
        .begin_control_turn(
            &ProjectId("project-a".into()),
            &SessionId("control-session".into()),
            &reopened_connection,
            &binding.routing_token,
            &grant.grant_id,
            &[delivery_token],
            "begin-host-b",
            now + TimeDelta::seconds(4),
        )
        .unwrap();
    assert!(matches!(begun, ControlTurnBeginDecision::Begin { .. }));
    let checkpointed = reopened
        .checkpoint_control_turn(
            &ProjectId("project-a".into()),
            &SessionId("control-session".into()),
            &reopened_connection,
            &binding.routing_token,
            &grant.grant_id,
            TurnNextIntent::Continue,
            "checkpoint-host-b",
            now + TimeDelta::seconds(5),
        )
        .unwrap();
    assert!(matches!(
        checkpointed,
        ControlTurnCheckpointDecision::Checkpointed { .. }
    ));

    let denied_intent = TurnIntent {
        idempotency_key: "host-turn-mutation".into(),
        intent_fingerprint: ObjectId::from_canonical_bytes(b"host-turn-mutation"),
        purpose: crate::domain::TurnPurpose::Ordinary,
        requested_effects: vec![EffectClass::MutateLocal],
        resource_intents: Vec::new(),
    };
    assert!(matches!(
        reopened
            .evaluate_control_turn(
                &ProjectId("project-a".into()),
                &SessionId("control-session".into()),
                &reopened_connection,
                &binding.routing_token,
                &denied_intent,
                now + TimeDelta::seconds(6),
            )
            .unwrap(),
        ControlTurnDecision::Refuse {
            directive: crate::domain::ControlDirective {
                code: crate::domain::ControlRefusalCode::ControlAssuranceInsufficient,
                ..
            }
        }
    ));
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the lease test preserves acquire, mutation, conflict, release, and fencing order"
)]
fn scoped_work_leases_gate_mutation_and_fence_transfer() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open_in_memory().unwrap();
    let effects = [
        EffectClass::Observe,
        EffectClass::Communicate,
        EffectClass::MutateLocal,
    ];
    let session_a = bind_control_for(&mut store, "lease-a", "bind-lease-a", &effects, now);
    let session_b = bind_control_for(&mut store, "lease-b", "bind-lease-b", &effects, now);
    complete_control_turn(
        &mut store,
        &session_a,
        "sync-lease-a",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(1),
    );
    let lease_subject = crate::domain::ResourceSubject::Path {
        project_id: ProjectId("project-a".into()),
        segments: vec!["src".into()],
        coverage: crate::domain::ResourceCoverage::Tree,
    };
    let lease_a = store
        .acquire_work_lease(
            &ProjectId("project-a".into()),
            &session_a.status.session_id,
            &session_a.connection_token,
            &session_a.routing_token,
            crate::domain::LeaseKind::Execution,
            crate::domain::LeaseMode::Exclusive,
            &lease_subject,
            300,
            "lease-src-a",
            now + TimeDelta::seconds(2),
        )
        .unwrap();
    let WorkLeaseDecision::Granted { lease: lease_a } = lease_a else {
        panic!("first non-conflicting lease must grant");
    };
    assert_projection_bytes(
        &store,
        "SELECT lease_json FROM control_work_leases WHERE lease_id = ?1",
        [&lease_a.lease_id],
        &lease_a,
    );
    assert_eq!(
        store
            .acquire_work_lease(
                &ProjectId("project-a".into()),
                &session_a.status.session_id,
                &session_a.connection_token,
                &session_a.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &lease_subject,
                300,
                "lease-src-a",
                now + TimeDelta::seconds(2),
            )
            .unwrap(),
        WorkLeaseDecision::Granted {
            lease: lease_a.clone()
        }
    );
    assert!(matches!(
        store.acquire_work_lease(
            &ProjectId("project-a".into()),
            &session_a.status.session_id,
            &session_a.connection_token,
            &session_a.routing_token,
            crate::domain::LeaseKind::Execution,
            crate::domain::LeaseMode::Exclusive,
            &lease_subject,
            299,
            "lease-src-a",
            now + TimeDelta::seconds(2),
        ),
        Err(StoreError::ControlOperationIdempotencyConflict { .. })
    ));
    complete_control_turn(
        &mut store,
        &session_a,
        "mutate-lease-a",
        vec![EffectClass::MutateLocal],
        vec![crate::domain::ResourceSubject::Path {
            project_id: ProjectId("project-a".into()),
            segments: vec!["src".into(), "lib.rs".into()],
            coverage: crate::domain::ResourceCoverage::Exact,
        }],
        now + TimeDelta::seconds(3),
    );

    complete_control_turn(
        &mut store,
        &session_b,
        "sync-lease-b",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(4),
    );
    assert!(matches!(
        store
            .acquire_work_lease(
                &ProjectId("project-a".into()),
                &session_b.status.session_id,
                &session_b.connection_token,
                &session_b.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &lease_subject,
                300,
                "lease-src-b-conflict",
                now + TimeDelta::seconds(5),
            )
            .unwrap(),
        WorkLeaseDecision::Defer { .. }
    ));
    assert!(matches!(
        store.release_work_lease(
            &ProjectId("project-a".into()),
            &session_b.status.session_id,
            &session_b.connection_token,
            &session_b.routing_token,
            &lease_a.lease_id,
            "wrong-holder-release",
            now + TimeDelta::seconds(6),
        ),
        Err(StoreError::WorkLeaseNotHeld { .. })
    ));
    let released = store
        .release_work_lease(
            &ProjectId("project-a".into()),
            &session_a.status.session_id,
            &session_a.connection_token,
            &session_a.routing_token,
            &lease_a.lease_id,
            "release-src-a",
            now + TimeDelta::seconds(6),
        )
        .unwrap();
    let mut terminal_lease = lease_a.clone();
    terminal_lease.revision += 1;
    assert_projection_bytes(
        &store,
        "SELECT lease_json FROM control_work_leases WHERE lease_id = ?1",
        [&lease_a.lease_id],
        &terminal_lease,
    );
    assert_projection_bytes(
        &store,
        "SELECT result_json FROM control_operation_results
         WHERE session_id = ?1 AND operation = 'lease_release' AND idempotency_key = ?2",
        [&session_a.status.session_id.0, "release-src-a"],
        &released,
    );
    assert_eq!(
        store
            .release_work_lease(
                &ProjectId("project-a".into()),
                &session_a.status.session_id,
                &session_a.connection_token,
                &session_a.routing_token,
                &lease_a.lease_id,
                "release-src-a",
                now + TimeDelta::seconds(6),
            )
            .unwrap(),
        released
    );
    complete_control_turn(
        &mut store,
        &session_b,
        "resync-lease-b",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(7),
    );
    let transferred = store
        .acquire_work_lease(
            &ProjectId("project-a".into()),
            &session_b.status.session_id,
            &session_b.connection_token,
            &session_b.routing_token,
            crate::domain::LeaseKind::Execution,
            crate::domain::LeaseMode::Exclusive,
            &lease_subject,
            300,
            "lease-src-b",
            now + TimeDelta::seconds(8),
        )
        .unwrap();
    let WorkLeaseDecision::Granted { lease: lease_b } = transferred else {
        panic!("released scope must transfer");
    };
    assert_eq!(lease_b.fence, lease_a.fence + 1);
    assert_eq!(lease_b.holder, session_b.status.session_id);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the regression preserves two task bindings, conflict, release, and project-wide fence transfer in one sequence"
)]
fn resource_lease_conflicts_and_fences_span_tasks_within_a_project() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open_in_memory().unwrap();
    let effects = [EffectClass::Observe, EffectClass::MutateLocal];
    let session_a = bind_control_for_task(
        &mut store,
        "project-lease-a",
        "bind-project-lease-a",
        "dummy:PROJECT-LEASE-A",
        &effects,
        now,
    );
    let session_b = bind_control_for_task(
        &mut store,
        "project-lease-b",
        "bind-project-lease-b",
        "dummy:PROJECT-LEASE-B",
        &effects,
        now,
    );
    assert_ne!(session_a.status.task_id, session_b.status.task_id);
    complete_control_turn(
        &mut store,
        &session_a,
        "sync-project-lease-a",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(1),
    );
    complete_control_turn(
        &mut store,
        &session_b,
        "sync-project-lease-b",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(2),
    );
    let subject = crate::domain::ResourceSubject::Path {
        project_id: ProjectId("project-a".into()),
        segments: vec!["src".into(), "shared.rs".into()],
        coverage: crate::domain::ResourceCoverage::Exact,
    };
    let WorkLeaseDecision::Granted { lease: first } = store
        .acquire_work_lease(
            &ProjectId("project-a".into()),
            &session_a.status.session_id,
            &session_a.connection_token,
            &session_a.routing_token,
            crate::domain::LeaseKind::Execution,
            crate::domain::LeaseMode::Exclusive,
            &subject,
            300,
            "project-lease-first",
            now + TimeDelta::seconds(3),
        )
        .unwrap()
    else {
        panic!("the first task must acquire the project resource");
    };
    assert!(matches!(
        store
            .acquire_work_lease(
                &ProjectId("project-a".into()),
                &session_b.status.session_id,
                &session_b.connection_token,
                &session_b.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &subject,
                300,
                "project-lease-conflict",
                now + TimeDelta::seconds(4),
            )
            .unwrap(),
        WorkLeaseDecision::Defer {
            conflicting_lease_id,
            ..
        } if conflicting_lease_id == first.lease_id
    ));
    store
        .release_work_lease(
            &ProjectId("project-a".into()),
            &session_a.status.session_id,
            &session_a.connection_token,
            &session_a.routing_token,
            &first.lease_id,
            "release-project-lease-first",
            now + TimeDelta::seconds(5),
        )
        .unwrap();
    let WorkLeaseDecision::Granted { lease: second } = store
        .acquire_work_lease(
            &ProjectId("project-a".into()),
            &session_b.status.session_id,
            &session_b.connection_token,
            &session_b.routing_token,
            crate::domain::LeaseKind::Execution,
            crate::domain::LeaseMode::Exclusive,
            &subject,
            300,
            "project-lease-second",
            now + TimeDelta::seconds(6),
        )
        .unwrap()
    else {
        panic!("the second task must acquire the released project resource");
    };
    assert_eq!(second.task_id, session_b.status.task_id);
    assert_eq!(second.fence, first.fence + 1);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the expiry test preserves grant, expiry, unwind, and fence continuity"
)]
fn expired_lease_invalidates_unbegun_turn_and_preserves_fence_history() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open_in_memory().unwrap();
    let effects = [EffectClass::Observe, EffectClass::MutateLocal];
    let binding = bind_control_for(&mut store, "expiry-a", "bind-expiry-a", &effects, now);
    complete_control_turn(
        &mut store,
        &binding,
        "sync-expiry-a",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(1),
    );
    let subject = crate::domain::ResourceSubject::Path {
        project_id: ProjectId("project-a".into()),
        segments: vec!["src".into()],
        coverage: crate::domain::ResourceCoverage::Tree,
    };
    let WorkLeaseDecision::Granted { lease: first } = store
        .acquire_work_lease(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            crate::domain::LeaseKind::Execution,
            crate::domain::LeaseMode::Exclusive,
            &subject,
            5,
            "lease-expiry-first",
            now + TimeDelta::seconds(2),
        )
        .unwrap()
    else {
        panic!("initial lease must grant");
    };
    let decision = store
        .evaluate_control_turn(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &TurnIntent {
                idempotency_key: "turn-before-lease-expiry".into(),
                intent_fingerprint: ObjectId::from_canonical_bytes(b"turn-before-lease-expiry"),
                purpose: crate::domain::TurnPurpose::Ordinary,
                requested_effects: vec![EffectClass::MutateLocal],
                resource_intents: vec![crate::domain::ResourceSubject::Path {
                    project_id: ProjectId("project-a".into()),
                    segments: vec!["src".into(), "lib.rs".into()],
                    coverage: crate::domain::ResourceCoverage::Exact,
                }],
            },
            now + TimeDelta::seconds(3),
        )
        .unwrap();
    let ControlTurnDecision::Grant { grant } = decision else {
        panic!("live lease must authorize the mutation turn");
    };
    assert!(matches!(
        store
            .begin_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &grant.grant_id,
                &[],
                "begin-after-lease-expiry",
                now + TimeDelta::seconds(8),
            )
            .unwrap(),
        ControlTurnBeginDecision::Refuse {
            code: crate::domain::ControlRefusalCode::StaleFence
        }
    ));
    assert_eq!(
        store
            .control_status(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                now + TimeDelta::seconds(8),
            )
            .unwrap()
            .phase,
        SessionPhase::Ready
    );
    let head_before_takeover = ChangeCursor(
        store
            .connection
            .query_row(
                "SELECT COALESCE(MAX(task_cursor), 0) FROM task_changes
                 WHERE task_id = ?1",
                [binding.status.task_id.0.to_string()],
                |row| row.get(0),
            )
            .unwrap(),
    );
    let WorkLeaseDecision::Granted { lease: second } = store
        .acquire_work_lease(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            crate::domain::LeaseKind::Execution,
            crate::domain::LeaseMode::Exclusive,
            &subject,
            5,
            "lease-expiry-second",
            now + TimeDelta::seconds(9),
        )
        .unwrap()
    else {
        panic!("expired scope must be acquirable");
    };
    assert_eq!(second.fence, first.fence + 1);
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT state FROM control_work_leases WHERE lease_id = ?1",
                [&first.lease_id],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "expired"
    );
    let (confirmed, blocking): (i64, i64) = store
        .connection
        .query_row(
            "SELECT confirmed_cursor, blocking_watermark FROM control_sessions
             WHERE session_id = ?1",
            [&binding.status.session_id.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(confirmed, head_before_takeover.0);
    assert_eq!(blocking, head_before_takeover.0 + 2);
    let delta = store
        .task_delta(
            &ProjectId("project-a".into()),
            binding.status.task_id,
            &binding.status.session_id,
            "agent",
            head_before_takeover,
            10,
        )
        .unwrap();
    assert_eq!(delta.changes.len(), 2);
    let expired: WorkLeaseEvent = serde_json::from_value(delta.changes[0].object.clone()).unwrap();
    let acquired: WorkLeaseEvent = serde_json::from_value(delta.changes[1].object.clone()).unwrap();
    assert_eq!(expired.lease.lease_id, first.lease_id);
    assert_eq!(expired.transition, WorkLeaseTransition::Expired);
    assert_eq!(acquired.lease.lease_id, second.lease_id);
    assert_eq!(acquired.transition, WorkLeaseTransition::Acquired);
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
                5,
                "lease-expiry-second",
                now + TimeDelta::seconds(10),
            )
            .unwrap(),
        WorkLeaseDecision::Granted {
            lease: second.clone()
        }
    );
    assert!(matches!(
        store.release_work_lease(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &first.lease_id,
            "release-expired-first",
            now + TimeDelta::seconds(10),
        ),
        Err(StoreError::WorkLeaseExpired { .. })
    ));
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the hostile sequence proves a begun turn pins explicit release and expiry transfer until checkpoint"
)]
fn begun_mutation_turn_pins_its_lease_until_checkpoint() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("lease-pin.sqlite3");
    let mut store = SqliteStore::open(&database).unwrap();
    let effects = [EffectClass::Observe, EffectClass::MutateLocal];
    let session_a = bind_control_for(&mut store, "pin-a", "bind-pin-a", &effects, now);
    let session_b = bind_control_for(&mut store, "pin-b", "bind-pin-b", &effects, now);
    complete_control_turn(
        &mut store,
        &session_b,
        "sync-pin-b",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(1),
    );
    complete_control_turn(
        &mut store,
        &session_a,
        "sync-pin-a",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(2),
    );
    let subject = crate::domain::ResourceSubject::Path {
        project_id: ProjectId("project-a".into()),
        segments: vec!["src".into()],
        coverage: crate::domain::ResourceCoverage::Tree,
    };
    let WorkLeaseDecision::Granted { lease: first } = store
        .acquire_work_lease(
            &ProjectId("project-a".into()),
            &session_a.status.session_id,
            &session_a.connection_token,
            &session_a.routing_token,
            crate::domain::LeaseKind::Execution,
            crate::domain::LeaseMode::Exclusive,
            &subject,
            5,
            "pin-first",
            now + TimeDelta::seconds(3),
        )
        .unwrap()
    else {
        panic!("the first lease must grant");
    };
    let decision = store
        .evaluate_control_turn(
            &ProjectId("project-a".into()),
            &session_a.status.session_id,
            &session_a.connection_token,
            &session_a.routing_token,
            &TurnIntent {
                idempotency_key: "pin-mutation-turn".into(),
                intent_fingerprint: ObjectId::from_canonical_bytes(b"pin-mutation-turn"),
                purpose: crate::domain::TurnPurpose::Ordinary,
                requested_effects: vec![EffectClass::MutateLocal],
                resource_intents: vec![crate::domain::ResourceSubject::Path {
                    project_id: ProjectId("project-a".into()),
                    segments: vec!["src".into(), "lib.rs".into()],
                    coverage: crate::domain::ResourceCoverage::Exact,
                }],
            },
            now + TimeDelta::seconds(4),
        )
        .unwrap();
    let ControlTurnDecision::Grant { grant } = decision else {
        panic!("the mutation turn must grant");
    };
    let delivery_tokens = grant
        .delivery
        .iter()
        .map(|delivery| delivery.page.delivery_token.clone())
        .collect::<Vec<_>>();
    assert!(matches!(
        store
            .begin_control_turn(
                &ProjectId("project-a".into()),
                &session_a.status.session_id,
                &session_a.connection_token,
                &session_a.routing_token,
                &grant.grant_id,
                &delivery_tokens,
                "begin-pinned-turn",
                now + TimeDelta::seconds(5),
            )
            .unwrap(),
        ControlTurnBeginDecision::Begin { .. }
    ));
    drop(store);
    let mut store = SqliteStore::open(&database).expect("restart store");
    let resumed_connection = store
        .resume_control_connection(&session_a.status.session_id, now + TimeDelta::seconds(6))
        .expect("resume begun-turn holder");
    assert!(matches!(
        store.release_work_lease(
            &ProjectId("project-a".into()),
            &session_a.status.session_id,
            &resumed_connection,
            &session_a.routing_token,
            &first.lease_id,
            "release-while-pinned",
            now + TimeDelta::seconds(6),
        ),
        Err(StoreError::InvalidControlSession(message))
            if message.contains("checkpoint the turn")
    ));

    complete_control_turn(
        &mut store,
        &session_b,
        "sync-pin-b-after-acquire",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(7),
    );
    assert!(matches!(
        store
            .acquire_work_lease(
                &ProjectId("project-a".into()),
                &session_b.status.session_id,
                &session_b.connection_token,
                &session_b.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &subject,
                30,
                "pin-second-deferred",
                now + TimeDelta::seconds(9),
            )
            .unwrap(),
        WorkLeaseDecision::Defer {
            checkpoint_required: true,
            ..
        }
    ));
    assert!(matches!(
        store
            .checkpoint_control_turn(
                &ProjectId("project-a".into()),
                &session_a.status.session_id,
                &resumed_connection,
                &session_a.routing_token,
                &grant.grant_id,
                TurnNextIntent::Continue,
                "checkpoint-pinned-turn",
                now + TimeDelta::seconds(10),
            )
            .unwrap(),
        ControlTurnCheckpointDecision::Checkpointed { .. }
    ));
    complete_control_turn(
        &mut store,
        &session_b,
        "sync-pin-b-after-checkpoint",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(11),
    );
    let WorkLeaseDecision::Granted { lease: second } = store
        .acquire_work_lease(
            &ProjectId("project-a".into()),
            &session_b.status.session_id,
            &session_b.connection_token,
            &session_b.routing_token,
            crate::domain::LeaseKind::Execution,
            crate::domain::LeaseMode::Exclusive,
            &subject,
            30,
            "pin-second-granted",
            now + TimeDelta::seconds(12),
        )
        .unwrap()
    else {
        panic!("checkpointed expired scope must transfer");
    };
    assert_eq!(second.fence, first.fence + 1);
    assert_eq!(second.holder, session_b.status.session_id);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the hostile sequence checks aliases, project binding, rebind rollback, and release"
)]
fn resource_aliases_and_active_leases_remain_task_isolated() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open_in_memory_with_host_path_identity(Some(HostPathPolicy {
        case_fold_paths: true,
        windows_alias_rules: false,
    }))
    .unwrap();
    let effects = [EffectClass::Observe, EffectClass::MutateLocal];
    let session_a = bind_control_for(&mut store, "alias-a", "bind-alias-a", &effects, now);
    let session_b = bind_control_for(&mut store, "alias-b", "bind-alias-b", &effects, now);
    complete_control_turn(
        &mut store,
        &session_b,
        "sync-alias-b",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(1),
    );
    complete_control_turn(
        &mut store,
        &session_a,
        "sync-alias-a",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(2),
    );
    let composed = crate::domain::ResourceSubject::Path {
        project_id: ProjectId("project-a".into()),
        segments: vec!["\u{17f}rc".into(), "caf\u{e9}.rs".into()],
        coverage: crate::domain::ResourceCoverage::Exact,
    };
    let WorkLeaseDecision::Granted { lease } = store
        .acquire_work_lease(
            &ProjectId("project-a".into()),
            &session_a.status.session_id,
            &session_a.connection_token,
            &session_a.routing_token,
            crate::domain::LeaseKind::Execution,
            crate::domain::LeaseMode::Exclusive,
            &composed,
            300,
            "lease-alias-a",
            now + TimeDelta::seconds(4),
        )
        .unwrap()
    else {
        panic!("first normalized subject must grant");
    };
    complete_control_turn(
        &mut store,
        &session_b,
        "resync-alias-b",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(4),
    );
    let decomposed = crate::domain::ResourceSubject::Path {
        project_id: ProjectId("project-a".into()),
        segments: vec!["src".into(), "cafe\u{301}.rs".into()],
        coverage: crate::domain::ResourceCoverage::Exact,
    };
    assert!(matches!(
        store
            .acquire_work_lease(
                &ProjectId("project-a".into()),
                &session_b.status.session_id,
                &session_b.connection_token,
                &session_b.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &decomposed,
                300,
                "lease-alias-b",
                now + TimeDelta::seconds(5),
            )
            .unwrap(),
        WorkLeaseDecision::Defer { .. }
    ));
    let wrong_project = crate::domain::ResourceSubject::Path {
        project_id: ProjectId("project-b".into()),
        segments: vec!["src".into()],
        coverage: crate::domain::ResourceCoverage::Tree,
    };
    assert!(matches!(
        store.acquire_work_lease(
            &ProjectId("project-a".into()),
            &session_b.status.session_id,
            &session_b.connection_token,
            &session_b.routing_token,
            crate::domain::LeaseKind::Execution,
            crate::domain::LeaseMode::Exclusive,
            &wrong_project,
            300,
            "lease-wrong-project",
            now + TimeDelta::seconds(6),
        ),
        Err(StoreError::InvalidControlSession(_))
    ));
    assert!(matches!(
        store.bind_control_session(
            &ProjectId("project-a".into()),
            "dummy:OTHER-TASK",
            "A different task",
            &session_a.status.session_id,
            &session_a.connection_token,
            &actor("alias-a"),
            ControlAssurance::TurnGated,
            &effects,
            1,
            "rebind-alias-a",
            now + TimeDelta::seconds(7),
        ),
        Err(StoreError::InvalidControlSession(_))
    ));
    assert_eq!(
        store
            .control_status(
                &ProjectId("project-a".into()),
                &session_a.status.session_id,
                &session_a.connection_token,
                &session_a.routing_token,
                now + TimeDelta::seconds(8),
            )
            .unwrap()
            .task_id,
        session_a.status.task_id
    );
    let rebound = store
        .bind_control_session(
            &ProjectId("project-a".into()),
            "dummy:OTHER-TASK",
            "A different task",
            &session_a.status.session_id,
            &session_a.connection_token,
            &actor("alias-a"),
            ControlAssurance::TurnGated,
            &effects,
            1,
            "rebind-after-expiry",
            now + TimeDelta::seconds(304),
        )
        .unwrap();
    assert_ne!(rebound.status.task_id, session_a.status.task_id);
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT state FROM control_work_leases WHERE lease_id = ?1",
                [&lease.lease_id],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "expired"
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the exit regression preserves synchronization, acquisition, checkpoint terminalization, and fenced reacquisition order"
)]
fn exiting_a_control_session_releases_its_leases_for_the_next_holder() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open_in_memory().unwrap();
    let effects = [EffectClass::Observe, EffectClass::MutateLocal];
    let session_a = bind_control_for(&mut store, "exit-a", "bind-exit-a", &effects, now);
    let session_b = bind_control_for(&mut store, "exit-b", "bind-exit-b", &effects, now);
    complete_control_turn(
        &mut store,
        &session_a,
        "sync-exit-a",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(1),
    );
    complete_control_turn(
        &mut store,
        &session_b,
        "sync-exit-b",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(2),
    );
    complete_control_turn(
        &mut store,
        &session_a,
        "resync-exit-a",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(3),
    );
    let subject = crate::domain::ResourceSubject::Path {
        project_id: ProjectId("project-a".into()),
        segments: vec!["src".into()],
        coverage: crate::domain::ResourceCoverage::Tree,
    };
    let WorkLeaseDecision::Granted { lease: first } = store
        .acquire_work_lease(
            &ProjectId("project-a".into()),
            &session_a.status.session_id,
            &session_a.connection_token,
            &session_a.routing_token,
            crate::domain::LeaseKind::Execution,
            crate::domain::LeaseMode::Exclusive,
            &subject,
            300,
            "lease-before-exit",
            now + TimeDelta::seconds(4),
        )
        .unwrap()
    else {
        panic!("the first session must acquire the lease");
    };
    let decision = store
        .evaluate_control_turn(
            &ProjectId("project-a".into()),
            &session_a.status.session_id,
            &session_a.connection_token,
            &session_a.routing_token,
            &TurnIntent {
                idempotency_key: "turn-before-exit".into(),
                intent_fingerprint: ObjectId::from_canonical_bytes(b"turn-before-exit"),
                purpose: crate::domain::TurnPurpose::Ordinary,
                requested_effects: vec![EffectClass::Observe],
                resource_intents: Vec::new(),
            },
            now + TimeDelta::seconds(5),
        )
        .unwrap();
    let ControlTurnDecision::Grant { grant } = decision else {
        panic!("the exit turn must grant");
    };
    let delivery_tokens = grant
        .delivery
        .iter()
        .map(|delivery| delivery.page.delivery_token.clone())
        .collect::<Vec<_>>();
    assert!(matches!(
        store
            .begin_control_turn(
                &ProjectId("project-a".into()),
                &session_a.status.session_id,
                &session_a.connection_token,
                &session_a.routing_token,
                &grant.grant_id,
                &delivery_tokens,
                "begin-before-exit",
                now + TimeDelta::seconds(6),
            )
            .unwrap(),
        ControlTurnBeginDecision::Begin { .. }
    ));
    assert!(matches!(
        store
            .checkpoint_control_turn(
                &ProjectId("project-a".into()),
                &session_a.status.session_id,
                &session_a.connection_token,
                &session_a.routing_token,
                &grant.grant_id,
                TurnNextIntent::Exit,
                "checkpoint-exit",
                now + TimeDelta::seconds(7),
            )
            .unwrap(),
        ControlTurnCheckpointDecision::Checkpointed {
            receipt: TurnCheckpointReceipt {
                phase: SessionPhase::Exited,
                ..
            }
        }
    ));
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT state FROM control_work_leases WHERE lease_id = ?1",
                [&first.lease_id],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "released"
    );
    complete_control_turn(
        &mut store,
        &session_b,
        "sync-after-exit",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(8),
    );
    let WorkLeaseDecision::Granted { lease: second } = store
        .acquire_work_lease(
            &ProjectId("project-a".into()),
            &session_b.status.session_id,
            &session_b.connection_token,
            &session_b.routing_token,
            crate::domain::LeaseKind::Execution,
            crate::domain::LeaseMode::Exclusive,
            &subject,
            300,
            "lease-after-exit",
            now + TimeDelta::seconds(9),
        )
        .unwrap()
    else {
        panic!("the second session must acquire the released scope");
    };
    assert_eq!(second.fence, first.fence + 1);
}

#[test]
fn task_only_control_checkpoint_cannot_append_execution_observations() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let binding = bind_control(&mut store, now);
    complete_control_turn(
        &mut store,
        &binding,
        "task-only-sync",
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
                idempotency_key: "task-only-observation-turn".into(),
                intent_fingerprint: ObjectId::from_canonical_bytes(b"task-only observation turn"),
                purpose: TurnPurpose::Ordinary,
                requested_effects: vec![EffectClass::Observe],
                resource_intents: Vec::new(),
            },
            now + TimeDelta::seconds(2),
        )
        .expect("evaluate task-only observation turn");
    let ControlTurnDecision::Grant { grant } = decision else {
        panic!("task-only observation turn should otherwise grant");
    };
    let delivery_tokens = grant
        .delivery
        .iter()
        .map(|delivery| delivery.page.delivery_token.clone())
        .collect::<Vec<_>>();
    assert!(matches!(
        store
            .begin_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &grant.grant_id,
                &delivery_tokens,
                "begin-task-only-observation",
                now + TimeDelta::seconds(3),
            )
            .expect("begin task-only observation turn"),
        ControlTurnBeginDecision::Begin { .. }
    ));
    let rejected = store.checkpoint_control_turn_with_observations(
        &ProjectId("project-a".into()),
        &binding.status.session_id,
        &binding.connection_token,
        &binding.routing_token,
        &grant.grant_id,
        TurnNextIntent::Continue,
        &[ExecutionObservationInput {
            observation_id: "task-only-observation".into(),
            action_fingerprint: ObjectId::from_canonical_bytes(b"read task context"),
            effect: EffectClass::Observe,
            outcome: crate::domain::ExecutionOutcome::Succeeded,
            source_changed: false,
            source_basis: None,
            observed_at: None,
        }],
        "checkpoint-task-only-observation",
        now + TimeDelta::seconds(4),
    );
    assert!(matches!(
        rejected,
        Err(StoreError::InvalidControlSession(message))
            if message.contains("local-work binding")
    ));
    let observations = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM objects WHERE object_kind = 'execution_observation'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("count task-only observations");
    assert_eq!(observations, 0);
}

#[test]
fn turn_gated_observe_only_session_cannot_reserve_undeclared_effects() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open_in_memory().expect("store");
    let binding = bind_control_for(
        &mut store,
        "observe-only-host",
        "bind-observe-only-host",
        &[EffectClass::Observe],
        now,
    );
    complete_control_turn(
        &mut store,
        &binding,
        "sync-observe-only-host",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(1),
    );
    let subject = crate::domain::ResourceSubject::Path {
        project_id: ProjectId("project-a".into()),
        segments: vec!["src".into()],
        coverage: crate::domain::ResourceCoverage::Tree,
    };

    for (kind, effect, key) in [
        (
            crate::domain::LeaseKind::Execution,
            EffectClass::MutateLocal,
            "observe-only-execution-lease",
        ),
        (
            crate::domain::LeaseKind::Coordination,
            EffectClass::Coordinate,
            "observe-only-coordination-lease",
        ),
    ] {
        let decision = store
            .acquire_work_lease(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                kind,
                crate::domain::LeaseMode::Exclusive,
                &subject,
                60,
                key,
                now + TimeDelta::seconds(2),
            )
            .expect("mediation refusal is a lease decision");
        let WorkLeaseDecision::Refuse { directive } = decision else {
            panic!("observe-only host must not reserve {effect:?}");
        };
        assert_eq!(
            directive.code,
            ControlRefusalCode::ControlAssuranceInsufficient
        );
        assert_eq!(directive.effect, Some(effect));
        assert_eq!(
            directive.required_assurance,
            Some(ControlAssurance::TurnGated)
        );
        assert_eq!(
            directive.declared_mediated_effects,
            Some(vec![EffectClass::Observe])
        );
        assert_eq!(
            directive.effective_mediated_effects,
            Some(vec![EffectClass::Observe])
        );
    }

    let turn = store
        .evaluate_control_turn(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &TurnIntent {
                idempotency_key: "observe-only-mutation-turn".into(),
                intent_fingerprint: ObjectId::from_canonical_bytes(b"observe-only-mutation-turn"),
                purpose: TurnPurpose::Ordinary,
                requested_effects: vec![EffectClass::MutateLocal],
                resource_intents: vec![subject],
            },
            now + TimeDelta::seconds(3),
        )
        .expect("turn mediation refusal");
    let ControlTurnDecision::Refuse { directive } = turn else {
        panic!("observe-only host must not receive a mutation turn");
    };
    assert_eq!(directive.effect, Some(EffectClass::MutateLocal));
    assert_eq!(
        directive.declared_mediated_effects,
        Some(vec![EffectClass::Observe])
    );
    assert_eq!(
        directive.effective_mediated_effects,
        Some(vec![EffectClass::Observe])
    );
}

#[test]
fn turn_gated_coordinate_session_can_acquire_a_coordination_lease() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open_in_memory().expect("store");
    let binding = bind_control_for(
        &mut store,
        "coordinate-host",
        "bind-coordinate-host",
        &[EffectClass::Observe, EffectClass::Coordinate],
        now,
    );
    complete_control_turn(
        &mut store,
        &binding,
        "sync-coordinate-host",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(1),
    );

    let decision = store
        .acquire_work_lease(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            crate::domain::LeaseKind::Coordination,
            crate::domain::LeaseMode::Exclusive,
            &crate::domain::ResourceSubject::Path {
                project_id: ProjectId("project-a".into()),
                segments: vec!["coordination".into()],
                coverage: crate::domain::ResourceCoverage::Tree,
            },
            60,
            "coordinate-lease",
            now + TimeDelta::seconds(2),
        )
        .expect("coordination lease decision");
    assert!(matches!(
        decision,
        WorkLeaseDecision::Granted { lease }
            if lease.kind == crate::domain::LeaseKind::Coordination
    ));
}

#[test]
fn lease_acquire_replay_is_scoped_to_the_current_bind_generation() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open_in_memory().expect("store");
    let binding = bind_control_for(
        &mut store,
        "lease-rebind-host",
        "bind-lease-rebind-host",
        &[EffectClass::Observe, EffectClass::MutateLocal],
        now,
    );
    complete_control_turn(
        &mut store,
        &binding,
        "sync-lease-rebind-host",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(1),
    );
    let subject = crate::domain::ResourceSubject::Path {
        project_id: ProjectId("project-a".into()),
        segments: vec!["src".into()],
        coverage: crate::domain::ResourceCoverage::Tree,
    };
    let WorkLeaseDecision::Granted { lease } = store
        .acquire_work_lease(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            crate::domain::LeaseKind::Execution,
            crate::domain::LeaseMode::Exclusive,
            &subject,
            60,
            "bind-scoped-acquire",
            now + TimeDelta::seconds(2),
        )
        .expect("initial lease")
    else {
        panic!("initial lease must grant");
    };
    store
        .release_work_lease(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &lease.lease_id,
            "release-before-rebind",
            now + TimeDelta::seconds(3),
        )
        .expect("release initial lease");
    let rebound = store
        .bind_control_session(
            &ProjectId("project-a".into()),
            "dummy:CONTROL-HOST-1",
            "Exercise the host control lifecycle",
            &binding.status.session_id,
            &binding.connection_token,
            &actor("lease-rebind-host"),
            ControlAssurance::TurnGated,
            &[EffectClass::Observe],
            2,
            "rebind-lease-rebind-host",
            now + TimeDelta::seconds(4),
        )
        .expect("rebind session");

    assert!(matches!(
        store.acquire_work_lease(
            &ProjectId("project-a".into()),
            &rebound.status.session_id,
            &binding.connection_token,
            &rebound.routing_token,
            crate::domain::LeaseKind::Execution,
            crate::domain::LeaseMode::Exclusive,
            &subject,
            60,
            "bind-scoped-acquire",
            now + TimeDelta::seconds(5),
        ),
        Err(StoreError::ControlOperationIdempotencyConflict { operation, key })
            if operation == "lease_acquire" && key == "bind-scoped-acquire"
    ));
}

#[test]
fn resume_control_connection_refuses_an_oversized_session_before_tx() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let giant = SessionId("c".repeat(65));
    let error = store
        .resume_control_connection(&giant, Utc.timestamp_millis_opt(1_700_000_000_000).unwrap())
        .expect_err("oversized control session");
    assert!(matches!(
        error,
        StoreError::InvalidWork(ref reason) if reason == crate::SessionIdAdmissionError::TooLong.as_str()
    ));
    assert!(!error.to_string().contains(&giant.0));
    let connections: i64 = store
        .connection
        .query_row("SELECT COUNT(*) FROM control_connections", [], |row| {
            row.get(0)
        })
        .expect("connections");
    assert_eq!(connections, 0);
}

#[test]
fn bind_control_session_refuses_an_oversized_session_before_effects() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let giant = SessionId("b".repeat(65));
    let sessions_before: i64 = store
        .connection
        .query_row("SELECT COUNT(*) FROM control_sessions", [], |row| {
            row.get(0)
        })
        .expect("sessions");
    let error = store
        .bind_control_session(
            &ProjectId("project-a".into()),
            "dummy:CONTROL-OVERSIZE",
            "Should not bind",
            &giant,
            "token",
            &actor(&giant.0),
            ControlAssurance::TurnGated,
            &[EffectClass::Observe],
            1,
            "bind-oversized",
            now,
        )
        .expect_err("oversized bind");
    assert!(matches!(
        error,
        StoreError::InvalidWork(ref reason)
            if reason == crate::SessionIdAdmissionError::TooLong.as_str()
    ));
    assert!(!error.to_string().contains(&giant.0));
    let sessions_after: i64 = store
        .connection
        .query_row("SELECT COUNT(*) FROM control_sessions", [], |row| {
            row.get(0)
        })
        .expect("sessions");
    assert_eq!(sessions_after, sessions_before);
}

fn bind_now() -> chrono::DateTime<Utc> {
    Utc.timestamp_millis_opt(1_700_000_000_000).unwrap()
}

fn refuse_start_or_join_before_effects(
    join: bool,
    participant: &SessionId,
    actor_session: Option<&SessionId>,
) {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let now = bind_now();
    let project = ProjectId("project-bind-admit".into());
    if join {
        store
            .start_task(
                &project,
                "dummy:BIND-ADMIT",
                "Seed task",
                &SessionId("starter".into()),
                actor("starter"),
                now,
            )
            .expect("seed task");
    }
    let before = crate::storage::test_database_shape_snapshot(&store.connection).expect("before");
    let mut ctx = actor("valid-actor");
    if let Some(session) = actor_session {
        ctx.session_id = Some(session.clone());
    }
    let error = if join {
        store.join_task(&project, "dummy:BIND-ADMIT", participant, ctx, now)
    } else {
        store.start_task(
            &project,
            "dummy:BIND-ADMIT",
            "Should not bind",
            participant,
            ctx,
            now,
        )
    }
    .expect_err("oversized bind identity");
    assert!(matches!(
        error,
        StoreError::InvalidWork(ref reason)
            if reason == crate::SessionIdAdmissionError::TooLong.as_str()
    ));
    let echoed = actor_session.unwrap_or(participant);
    assert!(!error.to_string().contains(&echoed.0));
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&store.connection).expect("after"),
        before,
        "oversized start/join must not persist tasks, participants, bindings, or events"
    );
}

#[test]
fn start_task_refuses_an_ascii65_participant_before_effects() {
    refuse_start_or_join_before_effects(false, &ascii65_session(), None);
}

#[test]
fn start_task_refuses_a_utf8_oversized_participant_before_effects() {
    refuse_start_or_join_before_effects(false, &utf8_oversized_session(), None);
}

#[test]
fn start_task_refuses_an_ascii65_actor_session_before_effects() {
    refuse_start_or_join_before_effects(
        false,
        &SessionId("valid-participant".into()),
        Some(&ascii65_session()),
    );
}

#[test]
fn start_task_refuses_a_utf8_oversized_actor_session_before_effects() {
    refuse_start_or_join_before_effects(
        false,
        &SessionId("valid-participant".into()),
        Some(&utf8_oversized_session()),
    );
}

#[test]
fn join_task_refuses_an_ascii65_participant_before_effects() {
    refuse_start_or_join_before_effects(true, &ascii65_session(), None);
}

#[test]
fn join_task_refuses_a_utf8_oversized_participant_before_effects() {
    refuse_start_or_join_before_effects(true, &utf8_oversized_session(), None);
}

#[test]
fn join_task_refuses_an_ascii65_actor_session_before_effects() {
    refuse_start_or_join_before_effects(
        true,
        &SessionId("valid-joiner".into()),
        Some(&ascii65_session()),
    );
}

#[test]
fn join_task_refuses_a_utf8_oversized_actor_session_before_effects() {
    refuse_start_or_join_before_effects(
        true,
        &SessionId("valid-joiner".into()),
        Some(&utf8_oversized_session()),
    );
}

#[test]
fn start_task_preserves_an_exact_64_byte_participant() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let session = exact_64_session(b'p');
    store
        .start_task(
            &ProjectId("project-bind-admit".into()),
            "dummy:BIND-64-P",
            "Max participant",
            &SessionId(session.clone()),
            actor("valid-actor"),
            bind_now(),
        )
        .expect("admitted participant");
    let stored: String = store
        .connection
        .query_row("SELECT session_id FROM task_participants", [], |row| {
            row.get(0)
        })
        .expect("participant");
    assert_eq!(stored, session);
}

#[test]
fn start_task_preserves_an_exact_64_byte_actor_session() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let session = exact_64_session(b'a');
    let mut ctx = actor("valid-actor");
    ctx.session_id = Some(SessionId(session.clone()));
    store
        .start_task(
            &ProjectId("project-bind-admit".into()),
            "dummy:BIND-64-A",
            "Max actor",
            &SessionId("valid-participant".into()),
            ctx,
            bind_now(),
        )
        .expect("admitted actor session");
    let bytes: Vec<u8> = store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_kind = 'task_started_event'",
            [],
            |row| row.get(0),
        )
        .expect("started event");
    let event: crate::domain::TaskStartedEvent = serde_json::from_slice(&bytes).expect("decode");
    assert_eq!(
        event.actor.session_id.as_ref().map(|id| id.0.as_str()),
        Some(session.as_str())
    );
}

#[test]
fn join_task_preserves_an_exact_64_byte_participant() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let now = bind_now();
    let project = ProjectId("project-bind-admit".into());
    store
        .start_task(
            &project,
            "dummy:BIND-64-JOIN-P",
            "Seed",
            &SessionId("starter".into()),
            actor("starter"),
            now,
        )
        .expect("seed");
    let session = exact_64_session(b'j');
    store
        .join_task(
            &project,
            "dummy:BIND-64-JOIN-P",
            &SessionId(session.clone()),
            actor("valid-actor"),
            now,
        )
        .expect("admitted join participant");
    let stored: String = store
        .connection
        .query_row(
            "SELECT session_id FROM task_participants WHERE session_id = ?1",
            [&session],
            |row| row.get(0),
        )
        .expect("joined participant");
    assert_eq!(stored, session);
}

#[test]
fn join_task_preserves_an_exact_64_byte_actor_session() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let now = bind_now();
    let project = ProjectId("project-bind-admit".into());
    store
        .start_task(
            &project,
            "dummy:BIND-64-JOIN-A",
            "Seed",
            &SessionId("starter".into()),
            actor("starter"),
            now,
        )
        .expect("seed");
    let session = exact_64_session(b'k');
    let mut ctx = actor("valid-actor");
    ctx.session_id = Some(SessionId(session.clone()));
    store
        .join_task(
            &project,
            "dummy:BIND-64-JOIN-A",
            &SessionId("valid-joiner".into()),
            ctx,
            now,
        )
        .expect("admitted join actor session");
    let bytes: Vec<u8> = store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_kind = 'task_joined_event'",
            [],
            |row| row.get(0),
        )
        .expect("joined event");
    let event: crate::domain::TaskJoinedEvent = serde_json::from_slice(&bytes).expect("decode");
    assert_eq!(
        event.actor.session_id.as_ref().map(|id| id.0.as_str()),
        Some(session.as_str())
    );
}
