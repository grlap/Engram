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
fn unresolved_path_identity_refuses_evaluation_without_writes_but_admits_logical_intents() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store =
        SqliteStore::open_in_memory_with_host_path_identity(None).expect("unresolved store");
    let binding = bind_control_for(
        &mut store,
        "unresolved-evaluate",
        "bind-unresolved-evaluate",
        &[EffectClass::Observe, EffectClass::MutateLocal],
        now,
    );
    complete_control_turn(
        &mut store,
        &binding,
        "unresolved-evaluate-sync",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(1),
    );
    let counts = |store: &SqliteStore| {
        store
            .connection
            .query_row(
                "SELECT (SELECT count(*) FROM control_turn_results),
                        (SELECT count(*) FROM control_turn_grants),
                        (SELECT count(*) FROM control_turn_grant_supersessions)",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .expect("control row counts")
    };
    let before = counts(&store);
    let intent = TurnIntent {
        idempotency_key: "unresolved-path-evaluate".into(),
        intent_fingerprint: ObjectId::from_canonical_bytes(b"unresolved-path-evaluate"),
        purpose: Some(TurnPurpose::Ordinary),
        requested_effects: vec![EffectClass::MutateLocal],
        resource_intents: vec![ResourceSubject::Path {
            project_id: ProjectId("project-a".into()),
            segments: vec!["src".into()],
            coverage: ResourceCoverage::Tree,
        }],
    };
    assert!(matches!(
        store.evaluate_control_turn(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &intent,
            now + TimeDelta::seconds(2),
        ),
        Err(StoreError::HostPathIdentityUnresolved)
    ));
    assert_eq!(counts(&store), before);

    let logical = ResourceSubject::Logical {
        namespace: "engram".into(),
        segments: vec!["report".into()],
        coverage: ResourceCoverage::Exact,
    };
    let grant = complete_control_turn(
        &mut store,
        &binding,
        "unresolved-logical-evaluate",
        vec![EffectClass::MutateLocal],
        vec![logical.clone()],
        now + TimeDelta::seconds(3),
    );
    assert_eq!(grant.basis.resource_intents, vec![logical]);
    assert_eq!(counts(&store), (before.0 + 1, before.1 + 1, before.2));
    assert!(
        store
            .verify_all()
            .expect("doctor after evaluation")
            .is_healthy()
    );
}

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
                    purpose: Some(TurnPurpose::Ordinary),
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
    assert_eq!(binding.status.phase, SessionPhase::Ready);
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
        .bind_test_control_scope(
            &ProjectId("project-a".into()),
            "dummy:CONTROL-HOST-1",
            "Peer control scope",
            &private_writer,
            &actor("private-writer"),
            now,
        )
        .expect("join a concurrent session for the same logical agent");

    let first_intent = TurnIntent {
        idempotency_key: "host-turn-a".into(),
        intent_fingerprint: ObjectId::from_canonical_bytes(b"host-turn-a"),
        purpose: Some(crate::domain::TurnPurpose::Ordinary),
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
        panic!("the first turn after bind must grant");
    };
    assert!(first_grant.delivery.is_none());
    // A host that still echoes a token from a delivery page is refused; the
    // grant stays issued until a fresh evaluation or connection retires it.
    let echoed_begin = store
        .begin_control_turn(
            &ProjectId("project-a".into()),
            &SessionId("control-session".into()),
            &binding.connection_token,
            &binding.routing_token,
            &first_grant.grant_id,
            &["token-from-an-old-page".into()],
            "begin-echoed-a",
            now + TimeDelta::seconds(2),
        )
        .unwrap();
    assert!(matches!(
        echoed_begin,
        ControlTurnBeginDecision::Refuse {
            code: crate::domain::ControlRefusalCode::GrantScopeMismatch
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
        purpose: Some(crate::domain::TurnPurpose::Ordinary),
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
        panic!("the first turn after a restart must grant");
    };
    assert!(grant.delivery.is_none());
    let begun = reopened
        .begin_control_turn(
            &ProjectId("project-a".into()),
            &SessionId("control-session".into()),
            &reopened_connection,
            &binding.routing_token,
            &grant.grant_id,
            &[],
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
        purpose: Some(crate::domain::TurnPurpose::Ordinary),
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

// Grants carry no delivery page, so neither a first bind, a re-bind over a
// new connection after a restart, nor task events written in between stand
// between a session and its first ordinary turn.
#[test]
fn bind_and_a_rebind_after_restart_are_ready_for_an_ordinary_turn_at_once() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let database = directory.path().join("engram.db");
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let effects = [EffectClass::Observe, EffectClass::Communicate];
    let mut store = SqliteStore::open(&database).expect("store");
    let first = bind_control(&mut store, now);
    assert_eq!(first.status.phase, SessionPhase::Ready);
    complete_control_turn(
        &mut store,
        &first,
        "after-bind",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(1),
    );
    store
        .bind_test_control_scope(
            &ProjectId("project-a".into()),
            "dummy:CONTROL-HOST-1",
            "Peer control scope",
            &SessionId("peer-session".into()),
            &actor("peer-session"),
            now + TimeDelta::seconds(2),
        )
        .expect("a peer joins the same task");
    store
        .capture_note(
            &note_request(
                first.status.task_id,
                "peer-session",
                "Decision: a peer writes before the restart.",
                "peer-before-restart",
                NoteVisibility::Shared,
            ),
            &DevelopmentNoopRedactor,
        )
        .expect("peer task event");
    drop(store);

    let mut reopened = SqliteStore::open(&database).expect("reopen store");
    let rebound = bind_control_for(
        &mut reopened,
        "control-session",
        "bind-control-after-restart",
        &effects,
        now + TimeDelta::seconds(3),
    );
    assert_eq!(rebound.status.task_id, first.status.task_id);
    assert_eq!(rebound.status.phase, SessionPhase::Ready);
    let grant = complete_control_turn(
        &mut reopened,
        &rebound,
        "after-restart",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(4),
    );
    assert!(grant.basis.delivery_cursor.is_none());
    let report = reopened.verify_all().expect("verify after rebind");
    assert!(report.is_healthy(), "{report:?}");
}

// Expiry of an issued grant returns the session to ready: there is no
// delivery to catch up on before the next turn.
#[test]
fn an_issued_grant_that_expires_while_turn_open_resets_the_session_to_ready() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open_in_memory().expect("store");
    let binding = bind_control(&mut store, now);
    let decision = store
        .evaluate_control_turn(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &TurnIntent {
                idempotency_key: "expires-unbegun".into(),
                intent_fingerprint: ObjectId::from_canonical_bytes(b"expires-unbegun"),
                purpose: None,
                requested_effects: vec![EffectClass::Observe],
                resource_intents: Vec::new(),
            },
            now + TimeDelta::seconds(1),
        )
        .expect("evaluate");
    let ControlTurnDecision::Grant { grant } = decision else {
        panic!("the turn must grant");
    };
    let open = store
        .control_status(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            now + TimeDelta::seconds(2),
        )
        .expect("status while issued");
    assert_eq!(open.phase, SessionPhase::TurnOpen);
    assert_eq!(open.open_grant_state, Some(TurnGrantState::Issued));

    let expired = store
        .control_status(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            grant.basis.expires_at + TimeDelta::seconds(1),
        )
        .expect("status after expiry");
    assert_eq!(expired.phase, SessionPhase::Ready);
    assert_eq!(expired.open_grant_id, None);
    complete_control_turn(
        &mut store,
        &binding,
        "after-expiry",
        vec![EffectClass::Observe],
        Vec::new(),
        grant.basis.expires_at + TimeDelta::seconds(2),
    );
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
                purpose: Some(TurnPurpose::Ordinary),
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
fn turn_gated_mutation_allows_empty_resource_intents_without_lease_basis() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open_in_memory().unwrap();
    let binding = bind_control_for(
        &mut store,
        "mutation-host",
        "bind-mutation-host",
        &[EffectClass::Observe, EffectClass::MutateLocal],
        now,
    );
    complete_control_turn(
        &mut store,
        &binding,
        "sync-mutation-host",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(1),
    );
    let grant = complete_control_turn(
        &mut store,
        &binding,
        "empty-intent-mutation",
        vec![EffectClass::MutateLocal],
        Vec::new(),
        now + TimeDelta::seconds(2),
    );
    assert_eq!(
        grant.basis.requested_effects,
        vec![EffectClass::MutateLocal]
    );
    assert!(grant.basis.resource_intents.is_empty());
    let value = serde_json::to_value(&grant).unwrap();
    assert!(value["basis"].as_object().unwrap().get("leases").is_none());
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn mutation_resource_intents_are_normalized_and_cross_project_intents_have_no_effects() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open_in_memory_with_host_path_identity(Some(HostPathPolicy {
        case_fold_paths: true,
        windows_alias_rules: false,
    }))
    .unwrap();
    let binding = bind_control_for(
        &mut store,
        "normalization-host",
        "bind-normalization-host",
        &[EffectClass::Observe, EffectClass::MutateLocal],
        now,
    );
    complete_control_turn(
        &mut store,
        &binding,
        "sync-normalization-host",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(1),
    );
    let subject =
        |project: &str, directory: &str, name: &str| crate::domain::ResourceSubject::Path {
            project_id: ProjectId(project.into()),
            segments: vec![directory.into(), name.into()],
            coverage: crate::domain::ResourceCoverage::Exact,
        };
    for (key, resource, seconds) in [
        (
            "decomposed",
            subject("project-a", "SRC", "Cafe\u{301}.RS"),
            2,
        ),
        ("case-alias", subject("project-a", "src", "CAFÉ.rs"), 3),
    ] {
        // Aliases belong in separate requests: duplicate normalized subjects
        // in one intent are invalid under the evaluator's uniqueness rule.
        let grant = complete_control_turn(
            &mut store,
            &binding,
            key,
            vec![EffectClass::MutateLocal],
            vec![resource],
            now + TimeDelta::seconds(seconds),
        );
        assert_eq!(
            grant.basis.resource_intents,
            vec![subject("project-a", "src", "café.rs")]
        );
    }
    let effects = |store: &SqliteStore| -> (i64, i64, i64) {
        store
            .connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM control_turn_results),
                    (SELECT COUNT(*) FROM control_turn_grants),
                    (SELECT COUNT(*) FROM control_turn_grant_supersessions)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap()
    };
    let before = effects(&store);
    let error = store
        .evaluate_control_turn(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &TurnIntent {
                idempotency_key: "cross-project-mutation".into(),
                intent_fingerprint: ObjectId::from_canonical_bytes(b"cross-project-mutation"),
                purpose: Some(TurnPurpose::Ordinary),
                requested_effects: vec![EffectClass::MutateLocal],
                resource_intents: vec![subject("project-b", "src", "main.rs")],
            },
            now + TimeDelta::seconds(4),
        )
        .unwrap_err();
    assert!(
        matches!(error, StoreError::InvalidControlSession(ref reason)
        if reason == "turn resource intent is invalid or belongs to another project")
    );
    assert_eq!(effects(&store), before);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn turn_gated_observe_only_session_refuses_undeclared_mutation() {
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

    let turn = store
        .evaluate_control_turn(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &TurnIntent {
                idempotency_key: "observe-only-mutation-turn".into(),
                intent_fingerprint: ObjectId::from_canonical_bytes(b"observe-only-mutation-turn"),
                purpose: Some(TurnPurpose::Ordinary),
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

#[test]
fn direct_binding_admits_session_byte_boundary_and_refuses_oversized_actor() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let project = ProjectId("project-a".into());
    for session in ["s".repeat(64), "é".repeat(32)] {
        let binding = store
            .bind_test_control_scope(
                &project,
                "boundary",
                "Boundary",
                &SessionId(session.clone()),
                &actor(&session),
                now,
            )
            .expect("64 UTF-8 bytes bind");
        assert_eq!(
            store.bound_task(&project, &SessionId(session)).unwrap(),
            binding.status.task_id
        );
        let before = crate::storage::test_database_shape_snapshot(&store.connection).unwrap();
        let oversized_session = "é".repeat(33);
        let error = store
            .bind_control_session(
                &project,
                "must-not-create",
                "Invalid actor",
                &binding.status.session_id,
                "unused: oversized actor refuses before connection lookup",
                &actor(&oversized_session),
                ControlAssurance::TurnGated,
                &[EffectClass::Observe],
                1,
                "invalid-actor",
                now,
            )
            .unwrap_err();
        assert!(matches!(error, StoreError::InvalidWork(ref reason)
            if reason == crate::SessionIdAdmissionError::TooLong.as_str()));
        assert!(!error.to_string().contains(&oversized_session));
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&store.connection).unwrap(),
            before
        );
    }
}

// Records written while grants carried a delivery page keep that shape: a
// grant basis with its purpose, cursors, watermark and inline page, the grant's
// delivery, the begin receipt's tentative cursor and the checkpoint event's
// delivered cursor. The fixture turns freshly stored records into that shape,
// field by field, and requires that they still decode and audit clean.
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one fixture reshapes every stored control record the page used to touch"
)]
fn records_stored_while_grants_carried_a_page_still_decode_and_audit_clean() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open_in_memory().expect("store");
    let binding = bind_control(&mut store, now);
    let grant = complete_control_turn(
        &mut store,
        &binding,
        "pre-change-shape",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(1),
    );
    let task_id = grant.basis.task_id;

    // A context-only page issued at an empty task feed, built as the earlier
    // build built it, with its content digest derived by the same code.
    let context = ContextPacket {
        header: ContextPacketHeader {
            project_id: ProjectId("project-a".into()),
            task_id: Some(task_id),
            work_id: None,
            work_feed_heads: Vec::new(),
            project_context_revision: 1,
            private_context_revision: 1,
            packet_hash: ObjectId::mint(),
            event_cursor: ChangeCursor(0),
            proposed_count: 0,
            stale_count: 0,
        },
        pinned: Vec::new(),
        index: Vec::new(),
        omissions: Vec::new(),
        omission_summaries: Vec::new(),
    };
    let delta = TaskDelta {
        task_id,
        after: ChangeCursor(0),
        cursor: ChangeCursor(0),
        changes: Vec::new(),
    };
    let page = DeliveryPage {
        from_cursor: ChangeCursor(0),
        to_cursor: ChangeCursor(0),
        head_cursor: ChangeCursor(0),
        has_more: false,
        content_digest: crate::control::delivery_content_digest(Some(&context), &delta)
            .expect("content digest"),
        delivery_token: "pre-change-delivery-token".into(),
    };
    let delivery = ControlDelivery {
        page: page.clone(),
        context: Some(context),
        delta,
    };
    let with_page = |mut grant: serde_json::Value| {
        let basis = grant["basis"].as_object_mut().expect("grant basis");
        basis.insert("purpose".into(), serde_json::json!("ordinary"));
        basis.insert("confirmed_cursor".into(), serde_json::json!(0));
        basis.insert("delivery_cursor".into(), serde_json::json!(0));
        basis.insert("blocking_watermark".into(), serde_json::json!(0));
        basis.insert(
            "inline_delivery".into(),
            serde_json::to_value(&page).expect("page"),
        );
        grant.as_object_mut().expect("grant").insert(
            "delivery".into(),
            serde_json::to_value(&delivery).expect("delivery"),
        );
        grant
    };
    let rewrite = |store: &SqliteStore, sql: &str, bytes: Vec<u8>, key: &str| {
        assert_eq!(
            store
                .connection
                .execute(sql, params![bytes, key])
                .expect("reshape a stored record"),
            1
        );
    };
    let canonical = |value: &serde_json::Value| {
        crate::canonical::canonical_bytes(value).expect("canonical bytes")
    };

    let stored_grant: Vec<u8> = store
        .connection
        .query_row(
            "SELECT grant_json FROM control_turn_grants WHERE grant_id = ?1",
            [&grant.grant_id],
            |row| row.get(0),
        )
        .expect("stored grant");
    let old_grant = with_page(serde_json::from_slice(&stored_grant).expect("grant JSON"));
    rewrite(
        &store,
        "UPDATE control_turn_grants SET grant_json = ?1 WHERE grant_id = ?2",
        canonical(&old_grant),
        &grant.grant_id,
    );

    let stored_decision: Vec<u8> = store
        .connection
        .query_row(
            "SELECT decision_json FROM control_turn_results WHERE idempotency_key = ?1",
            [&grant.request_key],
            |row| row.get(0),
        )
        .expect("stored decision");
    let mut old_decision: serde_json::Value =
        serde_json::from_slice(&stored_decision).expect("decision JSON");
    old_decision["grant"] = with_page(old_decision["grant"].take());
    rewrite(
        &store,
        "UPDATE control_turn_results SET decision_json = ?1 WHERE idempotency_key = ?2",
        canonical(&old_decision),
        &grant.request_key,
    );

    let stored_begin: Vec<u8> = store
        .connection
        .query_row(
            "SELECT result_json FROM control_operation_results
             WHERE operation = 'turn_begin' AND idempotency_key = 'begin-pre-change-shape'",
            [],
            |row| row.get(0),
        )
        .expect("stored begin receipt");
    let mut old_begin: serde_json::Value =
        serde_json::from_slice(&stored_begin).expect("begin JSON");
    old_begin["receipt"]
        .as_object_mut()
        .expect("begin receipt")
        .insert("tentative_cursor".into(), serde_json::json!(0));
    rewrite(
        &store,
        "UPDATE control_operation_results SET result_json = ?1
         WHERE operation = 'turn_begin' AND idempotency_key = ?2",
        canonical(&old_begin),
        "begin-pre-change-shape",
    );

    let (event_id, stored_event): (String, Vec<u8>) = store
        .connection
        .query_row(
            "SELECT object_id, canonical_json FROM objects
             WHERE object_kind = 'turn_checkpoint_event'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("stored checkpoint event");
    let mut old_event: serde_json::Value =
        serde_json::from_slice(&stored_event).expect("event JSON");
    assert!(old_event.get("delivered_cursor").is_none());
    old_event
        .as_object_mut()
        .expect("checkpoint event")
        .insert("delivered_cursor".into(), serde_json::json!(0));
    rewrite(
        &store,
        "UPDATE objects SET canonical_json = ?1 WHERE object_id = ?2",
        canonical(&old_event),
        &event_id,
    );

    let decoded: IssuedTurnGrant =
        SqliteStore::decode_json_projection(&canonical(&old_grant)).expect("old grant decodes");
    assert_eq!(decoded.basis.purpose, Some(TurnPurpose::Ordinary));
    assert_eq!(decoded.basis.delivery_cursor, Some(ChangeCursor(0)));
    assert_eq!(decoded.basis.blocking_watermark, Some(ChangeCursor(0)));
    assert_eq!(decoded.basis.inline_delivery, Some(page));
    assert!(decoded.delivery.is_some());
    assert!(crate::control::delivery_matches_grant(&decoded));
    let decision: ControlTurnDecision =
        SqliteStore::decode_json_projection(&canonical(&old_decision))
            .expect("old decision decodes");
    assert!(matches!(decision, ControlTurnDecision::Grant { .. }));
    let begin: ControlTurnBeginDecision =
        SqliteStore::decode_json_projection(&canonical(&old_begin)).expect("old begin decodes");
    assert!(matches!(
        begin,
        ControlTurnBeginDecision::Begin { receipt }
            if receipt.tentative_cursor == Some(ChangeCursor(0))
    ));
    let event: TurnCheckpointEvent =
        SqliteStore::decode_json_projection(&canonical(&old_event)).expect("old event decodes");
    assert_eq!(event.delivered_cursor, Some(ChangeCursor(0)));

    let report = store.verify_all().expect("audit the reshaped store");
    assert!(report.is_healthy(), "{report:?}");
    // The session keeps working on top of the earlier records.
    complete_control_turn(
        &mut store,
        &binding,
        "after-pre-change-shape",
        vec![EffectClass::Observe],
        Vec::new(),
        now + TimeDelta::seconds(2),
    );
}
