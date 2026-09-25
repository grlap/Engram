use chrono::TimeDelta;

use super::*;

use crate::{
    DevelopmentNoopRedactor,
    domain::{
        AssuranceLevel, ControlAssurance, EffectClass, NoteRequest, NoteVisibility, ProjectId,
        TurnIntent,
    },
};

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Example {
    pub(super) title: String,
    pub(super) body: String,
}

pub(super) struct SentinelRedactor;

pub(super) fn assert_projection_bytes<T: Serialize>(
    store: &SqliteStore,
    query: &str,
    parameters: impl rusqlite::Params,
    expected: &T,
) {
    let mut statement = store
        .connection
        .prepare(query)
        .expect("prepare stored projection query");
    let mut rows = statement
        .query_map(parameters, |row| row.get::<_, Vec<u8>>(0))
        .expect("query stored projection");
    let stored = rows
        .next()
        .expect("one stored projection")
        .expect("read stored projection");
    assert!(
        rows.next().is_none(),
        "expected exactly one stored projection"
    );
    // Derive the expected canonical form at runtime rather than pinning a digest.
    assert_eq!(stored, crate::canonical::canonical_bytes(expected).unwrap());
}

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

impl SqliteStore {
    pub(crate) fn bind_test_control_scope(
        &mut self,
        project: &ProjectId,
        external_ref: &str,
        title: &str,
        session: &SessionId,
        actor: &ActorContext,
        now: DateTime<Utc>,
    ) -> Result<ControlSessionBinding, StoreError> {
        let connection = self.resume_control_connection(session, now)?;
        self.bind_control_session(
            project,
            external_ref,
            title,
            session,
            &connection,
            actor,
            ControlAssurance::TurnGated,
            &[EffectClass::Observe, EffectClass::Communicate],
            1,
            &uuid::Uuid::new_v4().to_string(),
            now,
        )
    }
}

pub(super) fn install_memory_task(store: &mut SqliteStore, task_id: TaskId, sessions: &[&str]) {
    let external_ref = format!("memory-test:{task_id:?}");
    store
        .connection
        .execute(
            "INSERT INTO control_anchors (task_id, project_id, external_ref, title)
         VALUES (?1, 'project-a', ?2, 'Memory test')",
            params![task_id.0.to_string(), external_ref],
        )
        .expect("install memory scope");
    for session in sessions {
        store
            .bind_test_control_scope(
                &ProjectId("project-a".into()),
                &external_ref,
                "Memory test",
                &SessionId((*session).into()),
                &actor(session),
                Utc::now(),
            )
            .expect("bind memory scope");
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
                intent_fingerprint: ObjectId::from_canonical_bytes(key.as_bytes()),
                purpose: Some(crate::domain::TurnPurpose::Ordinary),
                requested_effects,
                resource_intents,
            },
            now,
        )
        .unwrap();
    let ControlTurnDecision::Grant { grant } = decision else {
        panic!("control turn {key} must grant");
    };
    assert!(grant.delivery.is_none(), "a grant carries no delivery page");
    assert!(matches!(
        store
            .begin_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &grant.grant_id,
                &[],
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

pub(super) fn ascii65_session() -> SessionId {
    SessionId("a".repeat(65))
}

pub(super) fn utf8_oversized_session() -> SessionId {
    SessionId("é".repeat(33))
}

pub(super) fn exact_64_session(byte: u8) -> String {
    String::from_utf8(vec![byte; crate::MAX_SESSION_ID_BYTES]).expect("ascii session")
}

pub(super) fn exact_64_ascii_session() -> String {
    exact_64_session(b'p')
}

pub(super) fn exact_64_utf8_session() -> String {
    "é".repeat(crate::MAX_SESSION_ID_BYTES / 2)
}

pub(super) fn assert_oversized_session_refusal(error: &StoreError, rejected: &SessionId) {
    assert!(matches!(
        error,
        StoreError::InvalidWork(reason)
            if reason == crate::SessionIdAdmissionError::TooLong.as_str()
    ));
    assert!(!error.to_string().contains(&rejected.0));
}
