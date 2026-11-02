use std::io::{self, Cursor, Write};

use crate::domain::*;
use crate::storage::{SqliteStore, test_database_shape_snapshot};
use crate::{DevelopmentNoopRedactor, host::HostControlServer};

use super::{CLOCK, ControlRequestScope, expire, now};

#[derive(Default)]
struct LostFlush(Vec<u8>);

impl Write for LostFlush {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "injected lost response",
        ))
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one real host transaction is observed before and after a lost reply and exact retry"
)]
fn ordinary_host_late_commit_with_lost_response_is_settled_by_exact_replay() {
    CLOCK.with(|clock| clock.set(std::time::Instant::now()));
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("late-commit.db");
    let mut store = SqliteStore::open(&database).unwrap();
    let wall = chrono::Utc::now();
    let session = SessionId("runner".into());
    let project = ProjectId("late-commit-project".into());
    let actor = crate::storage::test_support::actor(&session.0);
    let work = store
        .create_work(
            &CreateWorkRequest {
                external_ref: None,
                notes: vec![],
                project_id: project.clone(),
                parent_id: None,
                child_requirement: ChildRequirement::Required,
                title: "Observe committed work".into(),
                outcome: "Recover a lost response without repeating its effect".into(),
                acceptance: vec!["exact replay retains the original observation".into()],
                acceptance_bindings: vec![],
                kind: WorkItemKind::Task,
                priority: 1,
                labels: vec![],
                assigned_to: None,
                deferred_until: None,
                evaluation_mode: None,
                origin: WorkOrigin::Local,
                source_snapshot_id: None,
                actor: actor.clone(),
                idempotency_key: "create".into(),
                created_at: wall,
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let claim = store
        .claim_work(
            &ClaimWorkRequest {
                work_id: work.work_id,
                expected_work_revision: work.revision,
                expected_run_id: work.active_run_id,
                holder: session.clone(),
                ttl_seconds: 300,
                recovery_reason: None,
                actor,
                idempotency_key: "claim".into(),
                claimed_at: wall,
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let run = store.get_work_run(claim.run_id).unwrap();
    let cut = store
        .work_feed_head(&FeedId::RunExecution(run.run_id))
        .unwrap();
    let token = store.resume_work_connection(&session, wall).unwrap();
    let observer = rusqlite::Connection::open(&database).unwrap();
    let before = test_database_shape_snapshot(&observer).unwrap();
    store
        .connection
        .commit_hook(Some(|| {
            expire();
            false
        }))
        .unwrap();
    let mut server = HostControlServer::from_test_store(store, project, session, token);
    let request = serde_json::json!({
        "operation": "execution_observe", "idempotency_key": "lost-response",
        "binding": { "root_execution_id": run.root_execution_id, "work_id": work.work_id,
            "run_id": run.run_id, "work_revision": claim.accepted_work_revision,
            "claim_id": claim.claim_id, "claim_fence": claim.fence },
        "root_basis": {"capture_run_cut": cut, "latest_event": null, "state": {"state": "none"}},
        "observed_interval": {"from": wall, "through": wall},
        "occurrence": {"kind": "source_sighting", "source_basis": {
            "workspace_id": "measured-workspace", "source_revision": "measured-revision"}, "observed_at": wall},
        "causality": {"kind": "unknown"}, "policy_basis": {"mode": "audit_only"}
    });
    let mut input = serde_json::to_vec(&request).unwrap();
    input.push(b'\n');
    let mut lost = LostFlush::default();
    let started = now();
    let scope = ControlRequestScope::with_clock(now);
    let error = server
        .serve(Cursor::new(input.clone()), &mut lost)
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    let committed: serde_json::Value = serde_json::from_slice(&lost.0).unwrap();
    assert_eq!(committed["status"], "ok", "{committed}");
    assert!(now() >= started + super::CONTROL_REQUEST_ALLOWANCE);
    let settled = test_database_shape_snapshot(&observer).unwrap();
    assert_ne!(settled, before);
    drop(scope);
    let _fresh = ControlRequestScope::with_clock(now);
    let mut output = Vec::new();
    server.serve(Cursor::new(input), &mut output).unwrap();
    let replay: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(replay, committed);
    assert_eq!(test_database_shape_snapshot(&observer).unwrap(), settled);
    let observations: i64 = observer
        .query_row(
            "SELECT count(*) FROM objects WHERE object_kind = 'unadmitted_execution_observation'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(observations, 1);
}
