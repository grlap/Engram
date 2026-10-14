#[path = "../src/test_support.rs"]
mod test_support;

#[path = "common/task_sync_opt_in.rs"]
mod task_sync_opt_in;

use engram::task_sync::directory;
use serde_json::Value;
use std::{fs, path::Path, process::Command};

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

#[test]
fn task_prepare_cli_complete_candidates_profiles_and_process_lock() {
    let fixture = test_support::temp_home().unwrap();
    let marker = fixture.path().join(".engram-project");
    fs::write(&marker, "prepare-cli\n").unwrap();
    let home = fixture.path().join("home");
    task_sync_opt_in::enable(&home, "prepare-cli");
    let policy = if cfg!(windows) {
        "case_fold"
    } else {
        "case_sensitive"
    };
    let output = command(&home, &marker)
        .args(["--host-path-policy", policy, "init"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = command(&home, &marker)
        .args([
            "work",
            "--actor-id",
            "author",
            "--session-id",
            "author-session",
            "add",
            "Shared task",
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
    let prepare = |name: &str, flags: &[&str]| {
        command(&home, &marker)
            .current_dir(fixture.path())
            .args([
                "task-sync",
                "--actor-id",
                "operator",
                "--session-id",
                "operator-session",
                "prepare",
                name,
                "--progress-directory",
                "progress",
            ])
            .args(flags)
            .output()
            .unwrap()
    };
    let parse = |output: std::process::Output| -> Value {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("development no-op"));
        serde_json::from_slice(&output.stdout).unwrap()
    };
    let first = parse(prepare("first", &[]));
    assert_eq!(first["prepared"], true);
    assert_eq!(first["path"], "full");
    assert_eq!(first["serialized_tasks"], 1);
    let second = parse(prepare("second", &[]));
    assert_eq!(second["path"], "incremental");
    assert_eq!(second["serialized_tasks"], 0);
    assert_ne!(first["operation"], second["operation"]);
    assert!(
        first["activity_omissions"]["counts"]
            .as_object()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        directory::read(&fixture.path().join("first"))
            .unwrap()
            .documents()
            .unwrap(),
        directory::read(&fixture.path().join("second"))
            .unwrap()
            .documents()
            .unwrap()
    );
    let activity_only = parse(prepare(
        "activity-only",
        &["--origin-machine", "host-a", "--rebaseline"],
    ));
    assert_eq!(activity_only["path"], "full");
    assert!(
        !directory::read(&fixture.path().join("activity-only"))
            .unwrap()
            .project
            .notes_included
    );
    let flags = ["--include-notes", "--origin-machine", "host-a"];
    let refused = prepare("drift", &flags);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("pinned"));
    assert!(!fixture.path().join("drift").exists());
    let included = parse(prepare(
        "notes",
        &[
            "--include-notes",
            "--origin-machine",
            "host-a",
            "--rebaseline",
        ],
    ));
    assert_eq!(included["path"], "full");
    let tree = directory::read(&fixture.path().join("notes")).unwrap();
    assert_eq!(tree.tasks[0].notes[0].body, "Shared progress");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.path().join("progress/prepare.lock"))
        .unwrap();
    lock.lock().unwrap();
    let busy = prepare("busy", &flags);
    assert!(!busy.status.success());
    assert!(String::from_utf8_lossy(&busy.stderr).contains("another exporter holds"));
    assert!(!fixture.path().join("busy").exists());
    lock.unlock().unwrap();
    assert_eq!(parse(prepare("unlocked", &flags))["serialized_tasks"], 0);
    assert!(!fixture.path().join(".engram").exists());
}
