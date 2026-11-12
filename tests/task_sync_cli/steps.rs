use super::*;
use engram::{
    ActorContext, CanonicalObject, ObjectId, SessionId,
    task_sync::{
        data_worktree::{DataWorktree, StagedCommitRequest},
        ledger::{Binding, RepositoryBasis, RetainedCommit, StoreBasis, TargetBasis},
        progress::ExportCut,
    },
};
use serde_json::json;

#[path = "native_steps.rs"]
mod native_steps;

fn run(home: &Path, marker: &Path, operation: &str, request: &Value) -> Output {
    // A file also exercises the ordinary CLI's bounded @FILE parsing.
    let path = marker.parent().unwrap().join("selected-step.json");
    fs::write(&path, serde_json::to_vec(request).unwrap()).unwrap();
    command(home, marker)
        .env("ENGRAM_ACTOR_ID", "step-operator")
        .env("ENGRAM_SESSION_ID", "step-session")
        .args(["task-sync", operation, "--request"])
        .arg(format!("@{}", path.display()))
        .output()
        .unwrap()
}

fn success(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn fixture() -> (
    test_support::TempHome,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
    Binding,
) {
    let owned = test_support::temp_home().unwrap();
    let marker = owned.path().join(".engram-project");
    let project = ProjectId("step-cli".into());
    fs::write(&marker, &project.0).unwrap();
    let folder = data_worktree::initialize(&marker);
    let home = owned.path().join("home");
    task_sync_opt_in::enable(&home, &project.0);
    assert!(
        command(&home, &marker)
            .arg("init")
            .output()
            .unwrap()
            .status
            .success()
    );
    let database = project_database_path(&home, &project);
    let observed = DataWorktree::observe_project(&marker, &project, "engram-data").unwrap();
    let actor = ActorContext {
        actor_id: "step-operator".into(),
        actor_kind: "agent".into(),
        assurance: engram::domain::AssuranceLevel::Asserted,
        run_id: None,
        session_id: Some(SessionId("step-session".into())),
        source_tool: None,
        source_skill: None,
        provenance_chain: Vec::new(),
        reason: "explicit fixture activation".into(),
    };
    let binding = Binding {
        schema_version: 1,
        project: project.clone(),
        store: StoreBasis {
            database: database.clone(),
            origin: None,
            restore_occurrence: None,
            cut: ExportCut {
                position: 0,
                anchor: None,
            },
        },
        repository: RepositoryBasis {
            enrollment: uuid::Uuid::new_v4(),
            main_checkout: observed.checkout.clone(),
            git_directory: observed.git_directory.clone(),
        },
        branch: observed.reference.clone(),
        target: TargetBasis {
            remote: "origin".into(),
            reference: observed.reference.clone(),
            configuration_fingerprint: CanonicalObject::freeze(&"fixture target")
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
        actor,
        created_at: chrono::Utc::now(),
    };
    (owned, marker, folder, home, binding)
}

#[test]
fn explicit_steps_activate_replay_advance_only_data_and_read_canonical_history() {
    let (owned, marker, folder, home, binding) = fixture();
    let project = binding.project.clone();
    let database = binding.store.database.clone();
    let observed = DataWorktree::observe_project(&marker, &project, "engram-data").unwrap();
    let code_before =
        engram::source::inspect(owned.path(), std::time::Duration::from_secs(30)).source_id;
    assert!(code_before.is_some());
    let index_before = fs::read(owned.path().join(".git/index")).unwrap();
    let activation = json!({"action":"activate", "project_file":marker, "binding":binding});
    let first = success(&run(&home, &marker, "step", &activation));
    assert_eq!(success(&run(&home, &marker, "step", &activation)), first);
    let binding_id: ObjectId = serde_json::from_value(first["binding"].clone()).unwrap();
    assert_eq!(
        SqliteStore::open(&database)
            .unwrap()
            .task_sync_binding(&binding_id)
            .unwrap(),
        Some(binding.clone())
    );

    fs::create_dir(&folder).unwrap();
    fs::write(
        folder.join("project.json"),
        serde_json::to_vec(&json!({"format":engram::task_sync::FORMAT,
        "project_id":project, "notes_included":false}))
        .unwrap(),
    )
    .unwrap();
    data_worktree::git(&observed.checkout, &["add", "--", ".engram"]);
    let tree = String::from_utf8(data_worktree::git(&observed.checkout, &["write-tree"]))
        .unwrap()
        .trim()
        .to_owned();
    let commit = StagedCommitRequest {
        operation: uuid::Uuid::new_v4(),
        tree,
        expected_head: None,
        parents: Vec::new(),
        author_name: "Fixture".into(),
        author_email: "fixture@example.invalid".into(),
        authored_at: chrono::DateTime::from_timestamp(chrono::Utc::now().timestamp(), 0).unwrap(),
        message: "Retain task data\n".into(),
    };
    let retained: RetainedCommit =
        serde_json::from_value(success(&run(&home, &marker, "data-commit", &json!(commit))))
            .unwrap();
    let advance = json!({"action":"advance_data", "project_file":marker, "branch":"engram-data", "retained":retained, "expected_head":null});
    let first_advance = success(&run(&home, &marker, "step", &advance));
    assert_eq!(first_advance["head"], retained.oid);
    assert_eq!(
        success(&run(&home, &marker, "step", &advance)),
        first_advance
    );
    assert_eq!(
        fs::read(owned.path().join(".git/index")).unwrap(),
        index_before
    );
    assert!(
        engram::source::inspect(owned.path(), std::time::Duration::from_secs(30))
            .source_id
            .eq(&code_before)
    );
    assert!(!owned.path().join(".engram").exists());

    // Incoming audit uses the same CLI runtime. Its asynchronous admission
    // must return the project refusal, without a nested-runtime panic or write.
    let audit = engram::task_sync::delta::CustodyAuditRequest {
        binding: binding_id.clone(),
        operation: uuid::Uuid::new_v4(),
        key: "foreign incoming audit".into(),
        reason: engram::task_sync::delta::CheckpointReason::Bootstrap,
        previous_checkpoint: None,
        expected_tail: ObjectId::mint(),
        expected_local_cut: engram::task_sync::delta::OriginQualifiedCut {
            binding: binding_id.clone(),
            origin: ObjectId::mint(),
            restore_occurrence: None,
            position: engram::FeedPosition {
                feed: engram::FeedId::Project(ProjectId("another-project".into())),
                position: 0,
            },
            anchor: None,
        },
        seed: engram::task_sync::delta::CustodySeedSelection::IncomingDataCommit {
            commit: retained,
        },
    };
    let before_audit = fs::read(&database).unwrap();
    let refused = run(&home, &marker, "custody-audit", &json!(audit));
    assert!(!refused.status.success());
    assert_eq!(refused.stdout, b"");
    let error = String::from_utf8_lossy(&refused.stderr);
    assert!(
        error.contains("custody audit request belongs to another project"),
        "{error}"
    );
    assert!(!error.contains("panicked"), "{error}");
    assert_eq!(fs::read(&database).unwrap(), before_audit);

    // Historical reads do not depend on the current data checkout or opt-in.
    fs::remove_file(database.parent().unwrap().join("task-sync-setting.json")).unwrap();
    fs::rename(&observed.checkout, owned.path().join("retired-data")).unwrap();
    let read = json!({"action":"read_binding", "binding":binding_id});
    assert_eq!(
        success(&run(&home, &marker, "step", &read))["record"],
        json!(binding)
    );
    let mut malformed = read;
    malformed["enable"] = json!(true);
    let refused = run(&home, &marker, "step", &malformed);
    assert!(!refused.status.success());
    assert_eq!(refused.stdout, b"");
}
