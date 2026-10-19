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

fn success(command: &mut Command) -> Vec<u8> {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
fn task_sync_cli_activity_without_notes_preserves_origin_and_restores_no_authority() {
    let fixture = test_support::temp_home().unwrap();
    let marker = fixture.path().join(".engram-project");
    fs::write(&marker, "activity-cli\n").unwrap();
    let project = ProjectId("activity-cli".into());
    let source = fixture.path().join("source");
    let target = fixture.path().join("target");
    // SqliteStore::open below checks the store against the host default, so
    // initialize with that policy rather than guessing it per platform.
    let host_policy = if engram::HostPathPolicy::host_default().case_fold_paths {
        "case_fold"
    } else {
        "case_sensitive"
    };
    for home in [&source, &target] {
        task_sync_opt_in::enable(home, &project.0);
        success(command(home, &marker).args(["--host-path-policy", host_policy, "init"]));
    }
    let added: Value = serde_json::from_slice(&success(command(&source, &marker).args([
        "work",
        "--actor-id",
        "author",
        "--session-id",
        "worker",
        "add",
        "Activity example",
        "--note",
        "Shared progress",
        "--json",
    ])))
    .unwrap();
    let reference = added["work"]["short_ref"].as_str().unwrap();
    success(command(&source, &marker).args([
        "work",
        "--actor-id",
        "author",
        "--session-id",
        "worker",
        "claim",
        reference,
        "--ttl",
        "3600",
        "--json",
    ]));
    let candidate = fixture.path().join("candidate");
    let receipt: Value = serde_json::from_slice(&success(
        command(&source, &marker)
            .args(["task-sync", "export"])
            .arg(&candidate)
            .args(["--origin-machine", "machine-A"]),
    ))
    .unwrap();
    assert_eq!(receipt["notes_included"], false);
    assert_eq!(
        receipt["activity_omissions"]["counts"],
        serde_json::json!({})
    );
    let original = directory::read_folder(&candidate).unwrap().tree;
    assert_eq!(original.tasks[0].notes.len(), 0);
    assert_eq!(original.tasks[0].activity["machine-A"].len(), 1);
    let notice = original.tasks[0].activity["machine-A"]
        .values()
        .next()
        .unwrap();
    assert_eq!(
        notice.state,
        engram::task_sync::activity::ActivityState::Started
    );
    assert_eq!(notice.state_revision, 1);
    assert_eq!(notice.activity_id, notice.event_id);
    assert_eq!(notice.actor.actor_id, "author");
    let documents = original.documents().unwrap();
    for bytes in documents.values() {
        let text = std::str::from_utf8(bytes).unwrap();
        for private in [
            "claim_id",
            "session_id",
            "run_id",
            "fence",
            "expires_at",
            "omissions",
        ] {
            assert!(
                !text.contains(private),
                "shared document contains {private}"
            );
        }
    }
    success(command(&source, &marker).args([
        "work",
        "--actor-id",
        "author",
        "--session-id",
        "worker",
        "claim",
        reference,
        "--ttl",
        "7200",
        "--json",
    ]));
    let renewed = fixture.path().join("renewed");
    success(
        command(&source, &marker)
            .args(["task-sync", "export"])
            .arg(&renewed)
            .args(["--origin-machine", "machine-A"]),
    );
    assert_eq!(
        directory::read_folder(&renewed)
            .unwrap()
            .tree
            .documents()
            .unwrap(),
        documents
    );
    let import = |session: &str| -> Value {
        serde_json::from_slice(&success(
            command(&target, &marker)
                .args([
                    "task-sync",
                    "--actor-id",
                    "operator",
                    "--session-id",
                    session,
                    "import",
                ])
                .arg(&candidate)
                .arg("--empty-basis"),
        ))
        .unwrap()
    };
    // The same intent replays its first receipt; another session's identical
    // import is a new intent that finds nothing to change.
    let first = import("sync-operator");
    assert_eq!(first["changed"], true);
    assert_eq!(import("sync-operator"), first);
    assert_eq!(import("another-operator")["changed"], false);
    // The origin-machine alias is host-local: the importing host re-exports
    // the imported activity unchanged under its own alias, or under none.
    for alias in [None, Some("machine-A"), Some("machine-B")] {
        let path = fixture.path().join(alias.unwrap_or("plain"));
        let mut export = command(&target, &marker);
        export.args(["task-sync", "export"]).arg(&path);
        if let Some(alias) = alias {
            export.args(["--origin-machine", alias]);
        }
        success(&mut export);
        assert_eq!(
            directory::read_folder(&path)
                .unwrap()
                .tree
                .documents()
                .unwrap(),
            documents,
            "{alias:?}"
        );
    }
    let database = project_database_path(&target, &project);
    let store = SqliteStore::open(&database).unwrap();
    assert!(
        store
            .get_work_item(original.tasks[0].id)
            .unwrap()
            .active_run_id
            .is_none()
    );
    drop(store);
    let connection = rusqlite::Connection::open(database).unwrap();
    for table in ["work_runs", "work_claims", "work_completion_seals"] {
        let count: i64 = connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "informational import creates no {table}");
    }
    // The planning baseline is the only feed entry the import adds.
    let feed: Vec<String> = connection
        .prepare("SELECT object_kind FROM work_feed_entries")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(feed, ["task_sync_import_planning"]);
    let invalid = fixture.path().join("invalid-alias");
    let output = command(&source, &marker)
        .args(["task-sync", "export"])
        .arg(&invalid)
        .args(["--origin-machine", "../private"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!invalid.exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("origin-machine"));
}
