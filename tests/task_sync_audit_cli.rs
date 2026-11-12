#[path = "common/task_sync_opt_in.rs"]
mod task_sync_opt_in;
#[path = "../src/test_support.rs"]
mod test_support;

use engram::{
    ActorContext, CanonicalObject, FeedId, FeedPosition, ObjectId, ProjectId, SqliteStore,
    project_database_path,
    task_sync::{
        delta::{CheckpointReason, CustodyAuditRequest, CustodySeedSelection, OriginQualifiedCut},
        ledger::{Binding, RepositoryBasis, StoreBasis, TargetBasis},
        origin::EnrollRequest,
        progress::{ExportCut, ExportProfile},
    },
};
use std::{fs, path::Path, process::Command};

fn command(home: &Path, marker: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_engram"));
    command
        .arg("--home")
        .arg(home)
        .arg("--project-file")
        .arg(marker)
        .env("ENGRAM_ACTOR_ID", "audit-cli-operator")
        .env("ENGRAM_SESSION_ID", "audit-cli-session")
        .env_remove("ENGRAM_ACTOR_CONTEXT");
    command
}

#[test]
fn named_audit_cli_is_explicit_replays_exact_request_and_never_enables_managed_work() {
    let owned = test_support::temp_home().unwrap();
    let home = owned.path().join("home");
    let marker = owned.path().join(".engram-project");
    let project = ProjectId("audit-cli".into());
    fs::write(&marker, &project.0).unwrap();
    let missing = owned.path().join("missing-request.json");
    let refused = command(&home, &marker)
        .args(["task-sync", "custody-audit", "--request"])
        .arg(format!("@{}", missing.display()))
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("task sync is not enabled"));
    let database = project_database_path(&home, &project);
    assert!(!database.exists());
    task_sync_opt_in::enable(&home, &project.0);
    let mut store = SqliteStore::open(&database).unwrap();
    let now = chrono::Utc::now();
    let actor = ActorContext {
        actor_id: "audit-cli-operator".into(),
        actor_kind: "agent".into(),
        assurance: engram::domain::AssuranceLevel::Asserted,
        run_id: None,
        session_id: Some(engram::SessionId("audit-cli-session".into())),
        source_tool: Some("audit_cli_test".into()),
        source_skill: None,
        provenance_chain: Vec::new(),
        reason: "explicit audit fixture".into(),
    };
    let origin = store
        .enroll_task_sync_origin(
            &project,
            &actor,
            &EnrollRequest {
                operation: uuid::Uuid::new_v4(),
                predecessor: None,
            },
            now,
        )
        .unwrap();
    let cut = store
        .capture_task_changes(
            &project,
            &ExportCut {
                position: 0,
                anchor: None,
            },
        )
        .unwrap()
        .cut;
    let checkout = owned.path().join("checkout");
    let git_directory = checkout.join(".git");
    fs::create_dir_all(&git_directory).unwrap();
    let binding = Binding {
        schema_version: 1,
        project: project.clone(),
        store: StoreBasis {
            database: database.clone(),
            origin: Some(origin.origin_id.clone()),
            restore_occurrence: None,
            cut,
        },
        repository: RepositoryBasis {
            enrollment: uuid::Uuid::new_v4(),
            main_checkout: checkout,
            git_directory: git_directory.clone(),
        },
        branch: "refs/heads/main".into(),
        target: TargetBasis {
            remote: "origin".into(),
            reference: "refs/heads/main".into(),
            configuration_fingerprint: CanonicalObject::freeze(&uuid::Uuid::new_v4())
                .unwrap()
                .key()
                .clone(),
        },
        profile: ExportProfile {
            include_notes: false,
            origin_machine: None,
        },
        supersedes: None,
        superseded_tail: None,
        actor: actor.clone(),
        created_at: now,
    };
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let tail = store
        .task_sync_exchange_events(&id)
        .unwrap()
        .last()
        .unwrap()
        .0
        .clone();
    let cut = store
        .capture_task_changes(
            &project,
            &ExportCut {
                position: 0,
                anchor: None,
            },
        )
        .unwrap()
        .cut;
    let request = CustodyAuditRequest {
        binding: id.clone(),
        operation: uuid::Uuid::new_v4(),
        key: "named CLI audit".into(),
        reason: CheckpointReason::NamedAudit,
        previous_checkpoint: None,
        expected_tail: tail,
        expected_local_cut: OriginQualifiedCut {
            binding: id.clone(),
            origin: origin.origin_id,
            restore_occurrence: None,
            position: FeedPosition {
                feed: FeedId::Project(project),
                position: cut.position,
            },
            anchor: cut.anchor,
        },
        seed: CustodySeedSelection::CustodyOnly,
    };
    drop(store);
    let file = owned.path().join("audit-request.json");
    fs::write(&file, serde_json::to_vec(&request).unwrap()).unwrap();
    let run = || {
        command(&home, &marker)
            .args(["task-sync", "custody-audit", "--request"])
            .arg(format!("@{}", file.display()))
            .output()
            .unwrap()
    };
    let first = run();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(first["repeated"], false);
    assert_eq!(first["acknowledged"], true);
    let before = fs::read(&database).unwrap();
    let head = git_directory.join("engram-task-sync-custody-audits/head.json");
    let head_before = fs::read(&head).unwrap();
    let second = run();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let second: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(second["repeated"], true);
    assert_eq!(second["checkpoint"], first["checkpoint"]);
    assert_eq!(second["request_fingerprint"], first["request_fingerprint"]);
    assert_eq!(fs::read(&database).unwrap(), before);
    assert_eq!(fs::read(&head).unwrap(), head_before);
    assert!(
        !database
            .parent()
            .unwrap()
            .join("task-sync-custody-audit-pending.json")
            .exists()
    );
    let store = SqliteStore::open(&database).unwrap();
    assert_eq!(store.task_sync_exchange_events(&id).unwrap().len(), 2);
    assert!(store.verify_all().unwrap().is_healthy());
    let connection = rusqlite::Connection::open(&database).unwrap();
    let sources: i64 = connection
        .query_row(
            "SELECT count(*) FROM work_task_sync_source_heads",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sources, 0);
    let acknowledgement: serde_json::Value = serde_json::from_slice(&head_before).unwrap();
    assert_eq!(acknowledgement["mode"], "audit_only");
    let mut changed = request;
    changed.binding = ObjectId::mint();
    fs::write(&file, serde_json::to_vec(&changed).unwrap()).unwrap();
    assert!(!run().status.success());
}
