#[path = "../src/test_support.rs"]
mod test_support;

#[path = "common/task_sync_opt_in.rs"]
mod task_sync_opt_in;

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

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

fn sync_output(home: &Path, marker: &Path, operation: &str, path: &Path, extra: &[&str]) -> Output {
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
        .arg(path)
        .args(extra);
    command.output().unwrap()
}

fn sync(home: &Path, marker: &Path, operation: &str, path: &Path, extra: &[&str]) -> Value {
    let output = sync_output(home, marker, operation, path, extra);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn basis(folder: &Path) -> String {
    folder.join("project.json").to_str().unwrap().to_owned()
}

fn feed_rows(database: &Path, planning: bool) -> i64 {
    let connection = rusqlite::Connection::open(database).unwrap();
    let operator = if planning { "=" } else { "!=" };
    connection
        .query_row(
            &format!(
                "SELECT count(*) FROM work_feed_entries
                 WHERE object_kind {operator} 'task_sync_import_planning'"
            ),
            [],
            |row| row.get(0),
        )
        .unwrap()
}

fn receipts(database: &Path) -> i64 {
    rusqlite::Connection::open(database)
        .unwrap()
        .query_row("SELECT count(*) FROM work_operation_results", [], |row| {
            row.get(0)
        })
        .unwrap()
}

#[test]
fn task_sync_cli_round_trips_a_git_folder_without_creating_runtime_state() {
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
    // A source with no exchange history exports its whole planning.
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
    let original = directory::read_folder(&path).unwrap();
    assert_eq!(original.tree.tasks.len(), 1);
    assert_eq!(
        directory::read_folder_basis(&path.join("project.json"))
            .unwrap()
            .export_basis,
        original.export_basis
    );

    // A dry run performs the import and leaves nothing behind.
    let database = project_database_path(&target, &project);
    let no_receipts = receipts(&database);
    let dry = sync(
        &target,
        &marker,
        "import",
        &path,
        &["--empty-basis", "--dry-run"],
    );
    assert_eq!(
        (&dry["dry_run"], &dry["changed"]),
        (&Value::Bool(true), &Value::Bool(true))
    );
    let store = SqliteStore::open(&database).unwrap();
    assert_eq!(store.export_task_tree(&project).unwrap().tasks, []);
    drop(store);
    assert_eq!(feed_rows(&database, true), 0);
    assert_eq!(receipts(&database), no_receipts);

    let first = sync(&target, &marker, "import", &path, &["--empty-basis"]);
    assert_eq!(
        (&first["dry_run"], &first["changed"]),
        (&Value::Bool(false), &Value::Bool(true))
    );
    // The exact repeat replays its first receipt and records nothing more;
    // a dry run of it predicts that replay.
    assert_eq!(
        sync(&target, &marker, "import", &path, &["--empty-basis"]),
        first
    );
    let committed = receipts(&database);
    let mut predicted = first.clone();
    predicted["dry_run"] = Value::Bool(true);
    assert_eq!(
        sync(
            &target,
            &marker,
            "import",
            &path,
            &["--empty-basis", "--dry-run"]
        ),
        predicted
    );
    assert_eq!(feed_rows(&database, true), 1);
    assert_eq!(receipts(&database), committed);

    let exported = fixture.path().join("reexport");
    let receipt = sync(&target, &marker, "export", &exported, &["--json"]);
    let reexport = directory::read_folder(&exported).unwrap();
    assert_eq!(
        reexport.tree.documents().unwrap(),
        original.tree.documents().unwrap()
    );
    assert_eq!(receipt["export_basis"], reexport.export_basis.as_str());
    // Against this host's own basis an unchanged folder changes nothing; a dry
    // run on the enrolled store says so first and writes nothing.
    let dry = sync(
        &target,
        &marker,
        "import",
        &path,
        &["--basis", &basis(&exported), "--dry-run"],
    );
    assert_eq!(
        (&dry["dry_run"], &dry["changed"]),
        (&Value::Bool(true), &Value::Bool(false))
    );
    assert_eq!(feed_rows(&database, true), 1);
    assert_eq!(receipts(&database), committed);
    let unchanged = sync(
        &target,
        &marker,
        "import",
        &path,
        &["--basis", &basis(&exported)],
    );
    assert_eq!(unchanged["changed"], false);
    assert_eq!(feed_rows(&database, true), 1);

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
    let notes = directory::read_folder(&notes_path).unwrap();
    assert_eq!(notes.profile.origin_machine.as_deref(), Some("host-A"));
    assert_eq!(notes.tree.tasks[0].notes.len(), 1);
    assert_eq!(notes.tree.tasks[0].notes[0].body, "Shared progress");
    assert_eq!(notes.tree.tasks[0].notes[0].origin_machine, "host-A");
    assert!(
        !notes
            .tree
            .documents()
            .unwrap()
            .values()
            .any(|b| String::from_utf8_lossy(b).contains("omissions"))
    );
    let candidate_bytes =
        serde_json_canonicalizer::to_vec(&original.tree.documents().unwrap()).unwrap();
    eprintln!(
        "task-sync round-trip: tasks=1 kinds=task repeat_replayed=true directory_fingerprint={}",
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
    ] {
        let count: i64 = connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "import/export creates no {table}");
    }
    // The planning baseline is the only feed entry an import adds.
    assert_eq!(feed_rows(&database, false), 0);

    // The source's own basis still matches it, so its native work refuses.
    let output = sync_output(
        &source,
        &marker,
        "import",
        &path,
        &["--basis", &basis(&path)],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("native execution"));
}

#[test]
fn task_sync_hosts_with_distinct_aliases_exchange_folders() {
    let fixture = test_support::temp_home().unwrap();
    let marker = fixture.path().join(".engram-project");
    fs::write(&marker, "task-sync-hosts\n").unwrap();
    let [origin, host_a, host_b] = ["origin", "host-a", "host-b"].map(|name| {
        let home = fixture.path().join(format!("{name}-home"));
        task_sync_opt_in::enable(&home, "task-sync-hosts");
        let output = command(&home, &marker).arg("init").output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        home
    });
    let output = command(&origin, &marker)
        .args([
            "work",
            "--actor-id",
            "author",
            "--session-id",
            "author-session",
            "add",
            "shared task",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let first = fixture.path().join("first");
    sync(&origin, &marker, "export", &first, &["--json"]);

    // Each host enrols, then exports under its own alias.
    let folder_a = fixture.path().join("folder-a");
    sync(&host_a, &marker, "import", &first, &["--empty-basis"]);
    sync(
        &host_a,
        &marker,
        "export",
        &folder_a,
        &["--origin-machine", "host-A"],
    );
    let folder_b = fixture.path().join("folder-b");
    let enrolled = sync(&host_b, &marker, "import", &folder_a, &["--empty-basis"]);
    assert_eq!(enrolled["changed"], true);
    let receipt = sync(
        &host_b,
        &marker,
        "export",
        &folder_b,
        &["--origin-machine", "host-B"],
    );
    assert_eq!(
        directory::read_folder(&folder_b)
            .unwrap()
            .profile
            .origin_machine
            .as_deref(),
        Some("host-B")
    );
    assert_eq!(
        receipt["export_basis"],
        directory::read_folder(&folder_b)
            .unwrap()
            .export_basis
            .as_str()
    );

    // Each host imports the other's folder against its own basis, though the
    // folder names the other host's alias.
    for (home, folder, own) in [
        (&host_a, &folder_b, &folder_a),
        (&host_b, &folder_a, &folder_b),
    ] {
        let result = sync(home, &marker, "import", folder, &["--basis", &basis(own)]);
        assert_eq!(result["changed"], false);
    }

    // A renames the shared task and exports again; B takes the rename from
    // that folder against its own basis. A second rename follows on top of
    // the first, on a store that S1's empty-graph import would now refuse as
    // holding native state.
    let task = directory::read_folder(&folder_b).unwrap().tree.tasks[0].id;
    let mut own = folder_b.clone();
    for (round, title) in ["Renamed on host A", "Renamed twice on host A"]
        .into_iter()
        .enumerate()
    {
        let output = command(&host_a, &marker)
            .args([
                "work",
                "--actor-id",
                "author",
                "--session-id",
                "author-session",
                "update",
                &task.0.to_string(),
                "--title",
                title,
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let renamed = fixture.path().join(format!("folder-a-round-{round}"));
        sync(
            &host_a,
            &marker,
            "export",
            &renamed,
            &["--origin-machine", "host-A"],
        );
        let applied = sync(
            &host_b,
            &marker,
            "import",
            &renamed,
            &["--basis", &basis(&own)],
        );
        assert_eq!(applied["changed"], true, "{round}");
        own = fixture.path().join(format!("folder-b-round-{round}"));
        sync(
            &host_b,
            &marker,
            "export",
            &own,
            &["--origin-machine", "host-B"],
        );
        assert_eq!(
            directory::read_folder(&own).unwrap().tree.tasks[0].title,
            title
        );
    }
}

fn work(home: &Path, marker: &Path, args: &[&str]) -> Output {
    let output = command(home, marker)
        .args([
            "work",
            "--actor-id",
            "author",
            "--session-id",
            "author-session",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn task_sync_enrolled_hosts_exchange_new_tasks_both_ways_with_notes_and_activity() {
    let fixture = test_support::temp_home().unwrap();
    let marker = fixture.path().join(".engram-project");
    fs::write(&marker, "task-sync-adds\n").unwrap();
    let [host_a, host_b] = ["host-a", "host-b"].map(|name| {
        let home = fixture.path().join(format!("{name}-home"));
        task_sync_opt_in::enable(&home, "task-sync-adds");
        let output = command(&home, &marker).arg("init").output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        home
    });
    let notes_a = ["--include-notes", "--origin-machine", "host-A"];
    let notes_b = ["--include-notes", "--origin-machine", "host-B"];

    // Both hosts enrol on an empty folder that shares notes.
    let empty = fixture.path().join("empty");
    sync(&host_a, &marker, "export", &empty, &notes_a);
    for home in [&host_a, &host_b] {
        let enrolled = sync(home, &marker, "import", &empty, &["--empty-basis"]);
        assert_eq!(enrolled["changed"], true);
    }

    // A creates a task natively, claims it and records a shared note.
    work(&host_a, &marker, &["add", "native on A"]);
    let probe = fixture.path().join("probe-a");
    sync(&host_a, &marker, "export", &probe, &notes_a);
    let native = directory::read_folder(&probe).unwrap().tree.tasks[0].id;
    work(&host_a, &marker, &["claim", &native.0.to_string()]);
    work(&host_a, &marker, &["note", "Found the cause"]);
    let folder_a = fixture.path().join("folder-a");
    sync(&host_a, &marker, "export", &folder_a, &notes_a);
    let shared = directory::read_folder(&folder_a).unwrap().tree.tasks[0].clone();
    assert_eq!(shared.id, native);
    assert_eq!(shared.notes.len(), 1);
    assert!(!shared.activity.is_empty());

    // B adds it from A's folder, against its own basis.
    let own_b = fixture.path().join("own-b");
    sync(&host_b, &marker, "export", &own_b, &notes_b);
    let added = sync(
        &host_b,
        &marker,
        "import",
        &folder_a,
        &["--basis", &basis(&own_b)],
    );
    assert_eq!(added["changed"], true);

    // B creates its own task and exports both.
    work(&host_b, &marker, &["add", "native on B"]);
    let folder_b = fixture.path().join("folder-b");
    sync(&host_b, &marker, "export", &folder_b, &notes_b);
    let from_b = directory::read_folder(&folder_b).unwrap().tree;
    assert_eq!(
        from_b.tasks.iter().find(|task| task.id == native),
        Some(&shared)
    );
    let new_on_b = from_b
        .tasks
        .iter()
        .find(|task| task.id != native)
        .unwrap()
        .clone();

    // The return leg: A's own native task passes through unchanged, and B's
    // new task is restored on A.
    let returned = sync(
        &host_a,
        &marker,
        "import",
        &folder_b,
        &["--basis", &basis(&folder_a)],
    );
    assert_eq!(returned["changed"], true);
    let folder_a2 = fixture.path().join("folder-a-2");
    sync(&host_a, &marker, "export", &folder_a2, &notes_a);
    let on_a = directory::read_folder(&folder_a2).unwrap().tree;
    assert_eq!(
        on_a.tasks.iter().find(|task| task.id == native),
        Some(&shared)
    );
    assert_eq!(
        on_a.tasks.iter().find(|task| task.id == new_on_b.id),
        Some(&new_on_b)
    );
    let store = SqliteStore::open(project_database_path(
        &host_a,
        &ProjectId("task-sync-adds".into()),
    ))
    .unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
    drop(store);

    // An edit of A's native task, made on B, still refuses on A as
    // unsupported_origin: it was created on A, not restored by an import.
    work(
        &host_b,
        &marker,
        &["update", &native.0.to_string(), "--title", "Renamed on B"],
    );
    let renamed = fixture.path().join("folder-b-2");
    sync(&host_b, &marker, "export", &renamed, &notes_b);
    let output = sync_output(
        &host_a,
        &marker,
        "import",
        &renamed,
        &["--basis", &basis(&folder_a2), "--json"],
    );
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    let start = text
        .find("{\n")
        .unwrap_or_else(|| panic!("no JSON refusal on stderr: {text}"));
    let refused: Value = serde_json::from_str(&text[start..]).unwrap();
    assert_eq!(refused["error"]["code"], "task_sync_import_conflict");
    let message = refused["error"]["message"].as_str().unwrap();
    assert!(
        message.contains(&format!("{}: unsupported_origin", native.0)),
        "{message}"
    );
}

#[test]
fn task_sync_import_requires_exactly_one_destination_basis() {
    let fixture = test_support::temp_home().unwrap();
    let marker = fixture.path().join(".engram-project");
    fs::write(&marker, "task-sync-cli\n").unwrap();
    let home = fixture.path().join("home");
    let folder = fixture.path().join("folder");
    for extra in [&[][..], &["--empty-basis", "--basis", "project.json"][..]] {
        let output = sync_output(&home, &marker, "import", &folder, extra);
        assert_eq!(output.status.code(), Some(2), "{extra:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("--basis"), "{stderr}");
    }
    assert!(!home.exists(), "a usage error opens no store");
}
