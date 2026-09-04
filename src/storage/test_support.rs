use chrono::{TimeDelta, TimeZone};

use super::*;

use crate::{
    DevelopmentNoopRedactor,
    domain::{
        AssuranceLevel, ControlAssurance, ControlEpochs, ControlHealth, EffectClass, NoteRequest,
        NoteVisibility, PacketSafety, ProjectId, ProjectPolicyEpoch, SessionPhase,
        TaskAdmissionEpoch, TurnEvaluationInput, TurnIntent, TurnPurpose,
    },
};

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Example {
    pub(super) title: String,
    pub(super) body: String,
}

pub(super) struct SentinelRedactor;

impl Redactor for SentinelRedactor {
    fn inspect(&self, prose: &str) -> Result<(), String> {
        if prose.contains("reject-me") {
            Err("test sentinel was rejected".into())
        } else {
            Ok(())
        }
    }

    fn description(&self) -> &'static str {
        "test sentinel redactor"
    }
}

pub(super) fn actor(session: &str) -> ActorContext {
    ActorContext {
        actor_id: session.into(),
        actor_kind: "agent".into(),
        assurance: AssuranceLevel::Asserted,
        run_id: None,
        session_id: Some(SessionId(session.into())),
        source_tool: Some("test".into()),
        source_skill: None,
        provenance_chain: Vec::new(),
        reason: "exercise coordination semantics".into(),
    }
}

pub(super) fn open_with_assurance(
    path: &Path,
    required_assurance: ControlAssurance,
) -> Result<SqliteStore, StoreError> {
    SqliteStore::open_with_initial_control_assurance(
        path,
        Some(HostPathPolicy::host_default()),
        required_assurance,
        &actor("bootstrap-policy-admin"),
        "select the test bootstrap policy",
        &DevelopmentNoopRedactor,
    )
}

pub(super) fn note_request(
    task_id: TaskId,
    session: &str,
    prose: &str,
    key: &str,
    visibility: NoteVisibility,
) -> NoteRequest {
    NoteRequest {
        project_id: ProjectId("project-a".into()),
        task_id: Some(task_id),
        work_id: None,
        prose: prose.into(),
        visibility,
        kind: None,
        authority: None,
        sensitivity: None,
        title: None,
        tags: Vec::new(),
        evidence: Vec::new(),
        refs: Vec::new(),
        actor: actor(session),
        idempotency_key: key.into(),
        created_at: Utc::now(),
    }
}

pub(super) fn install_memory_task(store: &SqliteStore, task_id: TaskId, sessions: &[&str]) {
    let now = Utc::now().timestamp_millis();
    store
        .connection
        .execute(
            "INSERT INTO tasks (
                 task_id, project_id, external_ref, title, state,
                 event_cursor, created_at_ms, updated_at_ms
             ) VALUES (?1, 'project-a', ?2, 'Memory test', 'active', 0, ?3, ?3)",
            params![
                task_id.0.to_string(),
                format!("memory-test:{task_id:?}"),
                now
            ],
        )
        .expect("install memory test task");
    for session in sessions {
        store
            .connection
            .execute(
                "INSERT INTO task_participants (task_id, session_id, joined_at_ms)
                 VALUES (?1, ?2, ?3)",
                params![task_id.0.to_string(), session, now],
            )
            .expect("install memory test participant");
        store
            .connection
            .execute(
                "INSERT INTO session_bindings (session_id, task_id, bound_at_ms)
                 VALUES (?1, ?2, ?3)",
                params![session, task_id.0.to_string(), now],
            )
            .expect("install memory test binding");
    }
}

pub(super) fn turn_evaluation(task_id: TaskId) -> TurnEvaluationInput {
    TurnEvaluationInput {
        control_schema_version: crate::domain::CONTROL_SCHEMA_VERSION,
        session_id: SessionId("control-session".into()),
        task_id: Some(task_id),
        work_binding: None,
        work_binding_current: true,
        participant_membership: crate::domain::ParticipantMembership::Member,
        task_state: Some(TaskState::Active),
        phase: SessionPhase::Ready,
        health: ControlHealth::Healthy,
        active_policy_known: true,
        host_assurance: ControlAssurance::Advisory,
        required_assurance: ControlAssurance::Advisory,
        policy_effects: vec![
            EffectClass::Observe,
            EffectClass::Communicate,
            EffectClass::MutateLocal,
            EffectClass::MutateShared,
            EffectClass::ExternalSideEffect,
            EffectClass::Lifecycle,
        ],
        mediated_effects: vec![
            EffectClass::Observe,
            EffectClass::Communicate,
            EffectClass::MutateLocal,
            EffectClass::MutateShared,
            EffectClass::ExternalSideEffect,
            EffectClass::Lifecycle,
        ],
        current_epochs: ControlEpochs {
            project_policy: ProjectPolicyEpoch(1),
            task_admission: TaskAdmissionEpoch(2),
        },
        session_epochs: ControlEpochs {
            project_policy: ProjectPolicyEpoch(1),
            task_admission: TaskAdmissionEpoch(2),
        },
        confirmed_cursor: ChangeCursor(3),
        head_cursor: ChangeCursor(3),
        pending_delivery: None,
        packet_safety: PacketSafety::Safe,
        blocking_watermark: ChangeCursor(3),
        acknowledged_blocking_watermark: ChangeCursor(3),
        has_unknown_action_outcome: false,
        authority_satisfied: true,
        capability_map_revision: 1,
        leases: Vec::new(),
        intent: TurnIntent {
            idempotency_key: "observe-turn-a".into(),
            intent_fingerprint: ObjectHash::from_canonical_bytes(b"turn-a"),
            purpose: TurnPurpose::Ordinary,
            requested_effects: vec![EffectClass::Observe],
            resource_intents: Vec::new(),
        },
        evaluated_at: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
        grant_ttl_seconds: 30,
    }
}

pub(super) struct TestControlBinding {
    pub(super) binding: ControlSessionBinding,
    pub(super) connection_token: String,
}

impl std::ops::Deref for TestControlBinding {
    type Target = ControlSessionBinding;

    fn deref(&self) -> &Self::Target {
        &self.binding
    }
}

pub(super) fn bind_control(store: &mut SqliteStore, now: DateTime<Utc>) -> TestControlBinding {
    bind_control_for(
        store,
        "control-session",
        "bind-control-a",
        &[EffectClass::Observe, EffectClass::Communicate],
        now,
    )
}

pub(super) fn bind_control_for(
    store: &mut SqliteStore,
    session: &str,
    bind_key: &str,
    mediated_effects: &[EffectClass],
    now: DateTime<Utc>,
) -> TestControlBinding {
    bind_control_for_task(
        store,
        session,
        bind_key,
        "dummy:CONTROL-HOST-1",
        mediated_effects,
        now,
    )
}

pub(super) fn bind_control_for_task(
    store: &mut SqliteStore,
    session: &str,
    bind_key: &str,
    external_ref: &str,
    mediated_effects: &[EffectClass],
    now: DateTime<Utc>,
) -> TestControlBinding {
    let session_id = SessionId(session.into());
    let connection_token = store.resume_control_connection(&session_id, now).unwrap();
    let binding = store
        .bind_control_session(
            &ProjectId("project-a".into()),
            external_ref,
            "Exercise the host control lifecycle",
            &session_id,
            &connection_token,
            &actor(session),
            ControlAssurance::TurnGated,
            mediated_effects,
            1,
            bind_key,
            now,
        )
        .unwrap();
    TestControlBinding {
        binding,
        connection_token,
    }
}

pub(super) fn complete_control_turn(
    store: &mut SqliteStore,
    binding: &TestControlBinding,
    key: &str,
    requested_effects: Vec<EffectClass>,
    resource_intents: Vec<crate::domain::ResourceSubject>,
    now: DateTime<Utc>,
) -> IssuedTurnGrant {
    let decision = store
        .evaluate_control_turn(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &TurnIntent {
                idempotency_key: format!("turn-{key}"),
                intent_fingerprint: ObjectHash::from_canonical_bytes(key.as_bytes()),
                purpose: crate::domain::TurnPurpose::Ordinary,
                requested_effects,
                resource_intents,
            },
            now,
        )
        .unwrap();
    let ControlTurnDecision::Grant { grant } = decision else {
        panic!("control turn {key} must grant");
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
                &format!("begin-{key}"),
                now + TimeDelta::milliseconds(1),
            )
            .unwrap(),
        ControlTurnBeginDecision::Begin { .. }
    ));
    assert!(matches!(
        store
            .checkpoint_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &grant.grant_id,
                TurnNextIntent::Continue,
                &format!("checkpoint-{key}"),
                now + TimeDelta::milliseconds(2),
            )
            .unwrap(),
        ControlTurnCheckpointDecision::Checkpointed { .. }
    ));
    *grant
}
