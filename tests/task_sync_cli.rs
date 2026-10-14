#[path = "../src/test_support.rs"]
mod test_support;

#[path = "common/task_sync_opt_in.rs"]
mod task_sync_opt_in;

use std::{fs, path::Path, process::Command};

use engram::{ProjectId, SqliteStore, project_database_path, task_sync::directory};
use serde_json::Value;

fn command(home: &Path, marker: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_engram"));
    command
        .arg("--home")
        .arg(home)
        .arg("--project-file")
        .arg(marker)
        .env_remove("ENGRAM_ACTOR_CONTEXT");
    command
}

fn sync(home: &Path, marker: &Path, operation: &str, path: &Path, dry_run: bool) -> Value {
    let mut command = command(home, marker);
    command
        .env(
            "ENGRAM_ACTOR_CONTEXT",
            "injected task-sync operator context",
        )
        .args([
            "task-sync",
            "--actor-id",
            "operator",
            "--session-id",
            "sync-operator",
            operation,
        ])
        .arg(path);
    if dry_run {
        command.arg("--dry-run");
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn task_sync_cli_round_trips_relative_directories_without_creating_runtime_state() {
    let fixture = test_support::temp_home().unwrap();
    let marker = fixture.path().join(".engram-project");
    fs::write(&marker, "task-sync-cli\n").unwrap();
    let project = ProjectId("task-sync-cli".into());
    let source = fixture.path().join("source-home");
    let target = fixture.path().join("target-home");
    // SqliteStore::open below checks the store against the host default, so
    // initialize with that policy rather than guessing it per platform.
    let host_policy = if engram::HostPathPolicy::host_default().case_fold_paths {
        "case_fold"
    } else {
        "case_sensitive"
    };
    for home in [&source, &target] {
        task_sync_opt_in::enable(home, &project.0);
        let output = command(home, &marker)
            .args(["--host-path-policy", host_policy, "init"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let output = command(&source, &marker)
        .args([
            "work",
            "--actor-id",
            "author",
            "--session-id",
            "author-session",
            "add",
            "shared task",
            "--note",
            "Shared progress",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = command(&source, &marker)
        .current_dir(fixture.path())
        .args([
            "task-sync",
            "--actor-id",
            "operator",
            "--session-id",
            "sync-operator",
            "export",
            "candidate",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("development no-op"));
    assert!(!fixture.path().join(".engram").exists());
    let path = fixture.path().join("candidate");
    let original = directory::read(&path).unwrap().documents().unwrap();
    assert_eq!(
        sync(&target, &marker, "import", &path, true)["dry_run"],
        true
    );
    let database = project_database_path(&target, &project);
    let store = SqliteStore::open(&database).unwrap();
    assert_eq!(store.export_task_tree(&project).unwrap().tasks, []);
    drop(store);
    assert_eq!(
        sync(&target, &marker, "import", &path, false)["changed"],
        true
    );
    assert_eq!(
        sync(&target, &marker, "import", &path, false)["changed"],
        false
    );
    let exported = fixture.path().join("reexport");
    let output = command(&target, &marker)
        .args([
            "task-sync",
            "--actor-id",
            "operator",
            "--session-id",
            "sync-operator",
            "export",
        ])
        .arg(&exported)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        directory::read(&exported).unwrap().documents().unwrap(),
        original
    );
    let notes_path = fixture.path().join("with-notes");
    let output = command(&source, &marker)
        .args(["task-sync", "export"])
        .arg(&notes_path)
        .arg("--include-notes")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!notes_path.exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--origin-machine"));
    let output = command(&source, &marker)
        .args(["task-sync", "export"])
        .arg(&notes_path)
        .args(["--include-notes", "--origin-machine", "host-A"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["notes_included"], true);
    assert_eq!(receipt["omissions"]["counts"], serde_json::json!({}));
    let notes_tree = directory::read(&notes_path).unwrap();
    assert_eq!(notes_tree.tasks[0].notes.len(), 1);
    assert_eq!(notes_tree.tasks[0].notes[0].body, "Shared progress");
    assert_eq!(notes_tree.tasks[0].notes[0].origin_machine, "host-A");
    assert!(
        !notes_tree
            .documents()
            .unwrap()
            .values()
            .any(|b| String::from_utf8_lossy(b).contains("omissions"))
    );
    let candidate_bytes = serde_json_canonicalizer::to_vec(&original).unwrap();
    eprintln!(
        "task-sync round-trip: tasks=1 kinds=task repeat_changed=false directory_fingerprint={}",
        engram::ObjectId::from_canonical_bytes(&candidate_bytes)
    );
    let connection = rusqlite::Connection::open(&database).unwrap();
    let origin: Vec<u8> = connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_kind = 'task_sync_import_origin'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let origin: Value = serde_json::from_slice(&origin).unwrap();
    let actor: engram::ActorContext = serde_json::from_value(origin["actor"].clone()).unwrap();
    assert_eq!(
        actor.attribution_context(),
        Some("injected task-sync operator context")
    );
    for table in [
        "control_sessions",
        "work_session_state",
        "work_runs",
        "work_claims",
        "work_feed_entries",
    ] {
        let count: i64 = connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "import/export creates no {table}");
    }
    let output = command(&source, &marker)
        .args([
            "task-sync",
            "--actor-id",
            "operator",
            "--session-id",
            "sync-operator",
            "import",
        ])
        .arg(&path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("native execution"));
}
