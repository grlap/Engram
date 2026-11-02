use super::*;

use crate::domain::{AssuranceLevel, NoteRequest, NoteVisibility, ProjectId};

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
    pub(crate) fn test_is_autocommit(&self) -> bool {
        self.connection.is_autocommit()
    }

    pub(crate) fn test_set_busy_timeout(&self, timeout: Duration) -> rusqlite::Result<()> {
        self.connection.busy_timeout(timeout)
    }

    /// Installs passive historical task visibility for memory-reader fixtures.
    pub(crate) fn historical_task_scope(
        &mut self,
        project: &ProjectId,
        external_ref: &str,
        title: &str,
        session: &SessionId,
    ) -> Result<HistoricalTaskScope, StoreError> {
        self.connection.execute_batch("CREATE TEMP TABLE IF NOT EXISTS fixture_memory_membership (task_id TEXT NOT NULL, session_id TEXT NOT NULL, PRIMARY KEY(task_id,session_id))")?;
        let existing: Option<String> = self
            .connection
            .query_row(
                "SELECT task_id FROM control_anchors WHERE project_id = ?1 AND external_ref = ?2",
                params![project.0, external_ref],
                |row| row.get(0),
            )
            .optional()?;
        let task_id = if let Some(id) = existing {
            TaskId(uuid::Uuid::parse_str(&id).unwrap())
        } else {
            let id = TaskId::new();
            self.connection.execute("INSERT INTO control_anchors(task_id,project_id,external_ref,title) VALUES (?1,?2,?3,?4)", params![id.0.to_string(),project.0,external_ref,title])?;
            id
        };
        self.connection.execute(
            "INSERT OR IGNORE INTO fixture_memory_membership(task_id,session_id) VALUES (?1,?2)",
            params![task_id.0.to_string(), session.0],
        )?;
        Ok(HistoricalTaskScope { task_id })
    }
}

pub(crate) struct HistoricalTaskScope {
    pub(super) task_id: TaskId,
}

pub(super) fn historical_default_task(store: &mut SqliteStore) -> HistoricalTaskScope {
    store
        .historical_task_scope(
            &ProjectId("project-a".into()),
            "dummy:CONTROL-HOST-1",
            "Historical task",
            &SessionId("control-session".into()),
        )
        .unwrap()
}

pub(super) fn install_memory_task(store: &mut SqliteStore, task_id: TaskId, sessions: &[&str]) {
    let external_ref = format!("memory-test:{task_id:?}");
    store.connection.execute("INSERT INTO control_anchors(task_id,project_id,external_ref,title) VALUES (?1,'project-a',?2,'Memory test')", params![task_id.0.to_string(),external_ref]).unwrap();
    for session in sessions {
        store
            .historical_task_scope(
                &ProjectId("project-a".into()),
                &external_ref,
                "Memory test",
                &SessionId((*session).into()),
            )
            .unwrap();
    }
}

pub(super) struct TestWorkHost {
    pub(super) session_id: SessionId,
    pub(super) connection_token: String,
}

pub(super) fn open_work_host(
    store: &mut SqliteStore,
    session: &str,
    now: DateTime<Utc>,
) -> TestWorkHost {
    let session_id = SessionId(session.into());
    let connection_token = store.resume_work_connection(&session_id, now).unwrap();
    TestWorkHost {
        session_id,
        connection_token,
    }
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
