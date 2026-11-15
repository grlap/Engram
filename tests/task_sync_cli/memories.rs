use super::*;
use std::collections::BTreeMap;

fn success(output: Output) -> Output {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn setup() -> (
    test_support::TempHome,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let owned = test_support::temp_home().unwrap();
    let marker = owned.path().join(".engram-project");
    fs::write(&marker, "memory-cli-fixture\n").unwrap();
    data_worktree::initialize(&marker);
    let home = owned.path().join("store-home");
    task_sync_opt_in::enable(&home, "memory-cli-fixture");
    success(command(&home, &marker).arg("init").output().unwrap());
    (owned, marker, home)
}

fn work(home: &Path, marker: &Path, args: &[&str]) {
    success(
        command(home, marker)
            .args([
                "work",
                "--actor-id",
                "memory-fixture",
                "--session-id",
                "memory-session",
            ])
            .args(args)
            .output()
            .unwrap(),
    );
}

fn git(root: &Path, args: &[&str]) {
    success(
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
            ])
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", root.join("absent-config"))
            .output()
            .unwrap(),
    );
}

fn copy_tree(source: &Path, target: &Path) {
    fs::create_dir(target).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        assert!(!kind.is_symlink());
        if kind.is_dir() {
            copy_tree(&entry.path(), &target.join(entry.file_name()));
        } else {
            fs::copy(entry.path(), target.join(entry.file_name())).unwrap();
        }
    }
}

fn files(root: &Path) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    fn collect(
        root: &Path,
        directory: &Path,
        result: &mut std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>,
    ) {
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            assert!(!entry.file_type().unwrap().is_symlink());
            if entry.file_type().unwrap().is_dir() {
                collect(root, &entry.path(), result);
            } else {
                result.insert(
                    entry.path().strip_prefix(root).unwrap().to_owned(),
                    fs::read(entry.path()).unwrap(),
                );
            }
        }
    }
    let mut result = BTreeMap::default();
    collect(root, root, &mut result);
    result
}

#[test]
fn memory_sync_cli_explicit_retirement_reports_unseen_suffix_and_counterpart_retirement() {
    let (owned, marker, author_home) = setup();
    let peer_home = owned.path().join("peer-home");
    task_sync_opt_in::enable(&peer_home, "memory-cli-fixture");
    success(command(&peer_home, &marker).arg("init").output().unwrap());
    work(&author_home, &marker, &["remember", "r1", "--key", "rule"]);
    let folder = data_worktree::folder(&marker);
    let data = folder.parent().unwrap();
    let export_args = ["--include-memories", "--json"];
    let import_args = ["--include-memories", "--empty-basis", "--json"];
    success(sync_output(
        &author_home,
        &marker,
        "export",
        &folder,
        &export_args,
    ));
    git(data, &["add", ".engram"]);
    git(data, &["commit", "-m", "Record first revision"]);
    success(sync_output(
        &peer_home,
        &marker,
        "import",
        &folder,
        &import_args,
    ));
    work(
        &peer_home,
        &marker,
        &["remember", "r2", "--key", "rule", "--revise"],
    );
    success(sync_output(
        &peer_home,
        &marker,
        "export",
        &folder,
        &export_args,
    ));
    git(data, &["add", ".engram"]);
    git(data, &["commit", "-m", "Record peer suffix"]);
    let project = ProjectId("memory-cli-fixture".into());
    let store = SqliteStore::open(project_database_path(&peer_home, &project)).unwrap();
    let suffix = store
        .project_memory_sync_head(&project, "rule")
        .unwrap()
        .unwrap();
    drop(store);
    work(&author_home, &marker, &["forget", "rule"]);
    let before = files(&folder);
    let pending = sync_output(
        &author_home,
        &marker,
        "import",
        &folder,
        &["--include-memories", "--empty-basis", "--json", "--dry-run"],
    );
    assert_eq!(pending.status.code(), Some(3));
    let receipt: Value = serde_json::from_slice(&pending.stdout).unwrap();
    let conflict = &receipt["memories"][0];
    assert_eq!(conflict["status"], "memory_concurrent_retirement");
    assert_eq!(
        conflict["unseen_revisions"],
        serde_json::json!([suffix.clone()])
    );
    assert!(
        conflict["remedy"]
            .as_str()
            .unwrap()
            .contains("--retire-unseen rule")
    );
    let withheld = success(sync_output(
        &author_home,
        &marker,
        "export",
        &folder,
        &export_args,
    ));
    let receipt: Value = serde_json::from_slice(&withheld.stdout).unwrap();
    assert_eq!(receipt["memories"]["withheld"].as_array().unwrap().len(), 1);
    assert!(String::from_utf8_lossy(&withheld.stderr).contains("Memory key rule withheld:"));
    assert_eq!(files(&folder), before);
    let resolution_file = owned.path().join("retirement-options.json");
    fs::write(
        &resolution_file,
        serde_json::to_vec(&serde_json::json!({
            "rule": { "retire_unseen": true }
        }))
        .unwrap(),
    )
    .unwrap();
    let ungranted = success(sync_output(
        &author_home,
        &marker,
        "export",
        &folder,
        &[
            "--include-memories",
            "--memory-resolution",
            resolution_file.to_str().unwrap(),
            "--json",
        ],
    ));
    let receipt: Value = serde_json::from_slice(&ungranted.stdout).unwrap();
    assert_eq!(receipt["memories"]["withheld"].as_array().unwrap().len(), 1);
    assert_eq!(files(&folder), before);
    let resolved = success(sync_output(
        &author_home,
        &marker,
        "export",
        &folder,
        &["--include-memories", "--retire-unseen", "rule", "--json"],
    ));
    let receipt: Value = serde_json::from_slice(&resolved.stdout).unwrap();
    assert_eq!(
        receipt["memories"]["retired_unseen"][0]["revisions"],
        serde_json::json!([suffix.clone()])
    );
    git(data, &["add", ".engram"]);
    git(data, &["commit", "-m", "Explicitly retire unseen suffix"]);
    let received = success(sync_output(
        &peer_home,
        &marker,
        "import",
        &folder,
        &import_args,
    ));
    let receipt: Value = serde_json::from_slice(&received.stdout).unwrap();
    let retired = &receipt["memories"][0]["outcome"];
    assert_eq!(retired["retired"], true);
    assert!(
        retired["retired_local_revisions"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!(suffix))
    );
    assert_eq!(
        retired["retired_local_revisions"].as_array().unwrap().len(),
        2
    );
}

#[test]
fn memory_sync_cli_task_only_export_preserves_committed_memories() {
    let (_owned, marker, home) = setup();
    work(&home, &marker, &["remember", "retained", "--key", "rule"]);
    let folder = data_worktree::folder(&marker);
    success(sync_output(
        &home,
        &marker,
        "export",
        &folder,
        &["--include-memories", "--json"],
    ));
    git(folder.parent().unwrap(), &["add", ".engram"]);
    git(
        folder.parent().unwrap(),
        &["commit", "-m", "Record memory profile"],
    );
    let before = files(&folder);
    let result = sync_output(&home, &marker, "export", &folder, &["--json"]);
    assert_eq!(result.status.code(), Some(1));
    assert_eq!(
        serde_json::from_slice::<Value>(&result.stderr).unwrap()["error"]["code"],
        "memory_profile_requires_opt_in"
    );
    assert_eq!(files(&folder), before);
}

#[test]
fn memory_sync_cli_recovery_uses_current_task_envelope_after_task_change() {
    let (owned, marker, home) = setup();
    work(&home, &marker, &["add", "first task"]);
    work(
        &home,
        &marker,
        &["remember", "shared rule", "--key", "rule"],
    );
    let folder = data_worktree::folder(&marker);
    success(sync_output(
        &home,
        &marker,
        "export",
        &folder,
        &["--include-memories", "--json"],
    ));
    let retained = owned.path().join("retained-before");
    copy_tree(&folder, &retained);
    let data = folder.parent().unwrap();
    git(data, &["add", ".engram"]);
    git(data, &["commit", "-m", "Record first shared memory"]);
    let memory_before = files(&folder.join("memories"));
    work(&home, &marker, &["add", "second task"]);
    let project = folder.join("project.json");
    // Refresh through the supported CLI, then model the older capability-only
    // interruption with complete tasks and an envelope lacking the capability.
    success(sync_output(
        &home,
        &marker,
        "export",
        &folder,
        &["--include-memories", "--json"],
    ));
    let intended = fs::read(&project).unwrap();
    let mut envelope: Value = serde_json::from_slice(&intended).unwrap();
    envelope.as_object_mut().unwrap().remove("memory_profile");
    let mut current_tasks = serde_json::to_vec_pretty(&envelope).unwrap();
    current_tasks.push(b'\n');
    fs::write(&project, current_tasks).unwrap();
    let pending = folder.join(format!("project-{}.pending", uuid::Uuid::new_v4()));
    fs::write(&pending, &intended[..intended.len() / 2]).unwrap();
    let args = [
        "--include-memories",
        "--memory-recovery-basis",
        retained.to_str().unwrap(),
        "--json",
    ];
    success(sync_output(&home, &marker, "export", &folder, &args));
    assert!(!pending.exists());
    assert_eq!(fs::read(&project).unwrap(), intended);
    assert_eq!(files(&folder.join("memories")), memory_before);
    let observed = engram::task_sync::data_worktree::DataWorktree::observe(
        marker.parent().unwrap(),
        data,
        "engram-data",
    )
    .unwrap();
    assert_eq!(
        directory::read_resolved_data_folder_with_memories(&observed)
            .unwrap()
            .tree
            .tasks
            .len(),
        2
    );
    success(sync_output(&home, &marker, "export", &folder, &args));
}

#[test]
fn memory_sync_cli_uncommitted_memory_refusal_leaves_worktree_untouched() {
    let (_owned, marker, home) = setup();
    work(
        &home,
        &marker,
        &["remember", "shared rule", "--key", "rule"],
    );
    let folder = data_worktree::folder(&marker);
    success(sync_output(
        &home,
        &marker,
        "export",
        &folder,
        &["--include-memories", "--json"],
    ));
    work(&home, &marker, &["add", "unexported task"]);
    let before = files(&folder);
    let refused = sync_output(
        &home,
        &marker,
        "export",
        &folder,
        &["--include-memories", "--json"],
    );
    assert!(!refused.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&refused.stderr).unwrap()["error"]["code"],
        "memory_export_component_changed"
    );
    assert_eq!(files(&folder), before);
}

fn prepare_refresh_peer(
    author_home: &Path,
    marker: &Path,
    folder: &Path,
    owned: &Path,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let peer_home = owned.join("refresh-peer");
    task_sync_opt_in::enable(&peer_home, "memory-cli-fixture");
    success(command(&peer_home, marker).arg("init").output().unwrap());
    work(author_home, marker, &["add", "initial task"]);
    work(
        author_home,
        marker,
        &["remember", "first rule", "--key", "rule"],
    );
    let data = folder.parent().unwrap();
    let export_flags = ["--include-memories", "--json"];
    success(sync_output(
        author_home,
        marker,
        "export",
        folder,
        &export_flags,
    ));
    git(data, &["add", ".engram"]);
    git(data, &["commit", "-m", "Publish initial task and rule"]);
    success(sync_output(
        &peer_home,
        marker,
        "import",
        folder,
        &["--include-memories", "--empty-basis", "--json"],
    ));
    success(sync_output(
        &peer_home,
        marker,
        "export",
        folder,
        &export_flags,
    ));
    let peer_basis = owned.join("peer-project.json");
    fs::copy(folder.join("project.json"), &peer_basis).unwrap();
    (peer_home, peer_basis)
}

#[test]
fn memory_sync_cli_refresh_round_trips_changed_tasks_and_memory_history() {
    let (owned, marker, author_home) = setup();
    let folder = data_worktree::folder(&marker);
    let data = folder.parent().unwrap();
    let export_flags = ["--include-memories", "--json"];
    let (peer_home, peer_basis) =
        prepare_refresh_peer(&author_home, &marker, &folder, owned.path());
    let initial: Value =
        serde_json::from_slice(&fs::read(folder.join("project.json")).unwrap()).unwrap();
    let project = ProjectId("memory-cli-fixture".into());
    let store = SqliteStore::open(project_database_path(&author_home, &project)).unwrap();
    let captured = store
        .export_task_sync_folder(
            &project,
            &ExportProfile {
                include_notes: false,
                origin_machine: None,
            },
        )
        .unwrap();
    let identifier = captured.tree.tasks[0].id.0.to_string();
    drop(store);
    work(
        &author_home,
        &marker,
        &["update", &identifier, "--title", "changed task"],
    );
    work(
        &author_home,
        &marker,
        &[
            "remember",
            "revised rule",
            "--key",
            "rule",
            "--revise",
            "--expected-revision",
            "1",
        ],
    );
    let output = success(sync_output(
        &author_home,
        &marker,
        "export",
        &folder,
        &export_flags,
    ));
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["memories"]["exported_keys"], 1);
    assert_eq!(receipt["memories"]["withheld"].as_array().unwrap().len(), 0);
    assert_ne!(
        serde_json::from_slice::<Value>(&fs::read(folder.join("project.json")).unwrap()).unwrap(),
        initial
    );
    let published = files(&folder);
    success(sync_output(
        &author_home,
        &marker,
        "export",
        &folder,
        &export_flags,
    ));
    assert_eq!(files(&folder), published);
    git(data, &["add", ".engram"]);
    git(
        data,
        &["commit", "-m", "Publish changed task and revised rule"],
    );
    let import_flags = [
        "--include-memories",
        "--basis",
        peer_basis.to_str().unwrap(),
        "--json",
    ];
    let imported = success(sync_output(
        &peer_home,
        &marker,
        "import",
        &folder,
        &import_flags,
    ));
    let receipt: Value = serde_json::from_slice(&imported.stdout).unwrap();
    assert_eq!(receipt["memory_partial"], false);
    assert_eq!(receipt["memories"][0]["status"], "applied");
    assert_eq!(receipt["memories"][0]["outcome"]["appended_revisions"], 1);
    assert_refreshed_peer(&peer_home, &marker, &folder, &published);
}

fn assert_refreshed_peer(
    peer_home: &Path,
    marker: &Path,
    folder: &Path,
    published: &BTreeMap<std::path::PathBuf, Vec<u8>>,
) {
    let project = ProjectId("memory-cli-fixture".into());
    let peer = SqliteStore::open(project_database_path(peer_home, &project)).unwrap();
    let captured = peer
        .export_task_sync_folder(
            &project,
            &ExportProfile {
                include_notes: false,
                origin_machine: None,
            },
        )
        .unwrap();
    assert_eq!(captured.tree.tasks[0].title, "changed task");
    drop(peer);
    for (revision, body) in [("1", "first rule"), ("2", "revised rule")] {
        let read = success(
            command(peer_home, marker)
                .args([
                    "work",
                    "--actor-id",
                    "reader",
                    "--session-id",
                    "refresh-reader",
                    "--json",
                    "memories",
                    "rule",
                    "--full",
                    "--revision",
                    revision,
                ])
                .output()
                .unwrap(),
        );
        let read: Value = serde_json::from_slice(&read.stdout).unwrap();
        assert_eq!(read["body"], body);
        assert_eq!(read["reported_source"]["actor_id"], "memory-fixture");
    }
    success(sync_output(
        peer_home,
        marker,
        "export",
        folder,
        &["--include-memories", "--json"],
    ));
    assert_eq!(&files(folder), published);
}

fn publish_noted_author(author_home: &Path, marker: &Path, folder: &Path) -> String {
    work(author_home, marker, &["add", "shared task"]);
    work(
        author_home,
        marker,
        &["remember", "shared rule", "--key", "rule"],
    );
    let author_flags = [
        "--include-memories",
        "--include-notes",
        "--origin-machine",
        "host-a",
        "--json",
    ];
    success(sync_output(
        author_home,
        marker,
        "export",
        folder,
        &author_flags,
    ));
    let identifier = fs::read_dir(folder.join("tasks"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .file_stem()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    work(author_home, marker, &["claim", &identifier]);
    work(author_home, marker, &["note", "shared author note"]);
    // Commit the first export before refreshing its newly recorded note.
    let data = folder.parent().unwrap();
    git(data, &["add", ".engram"]);
    git(data, &["commit", "-m", "Publish first alias"]);
    success(sync_output(
        author_home,
        marker,
        "export",
        folder,
        &author_flags,
    ));
    git(data, &["add", ".engram"]);
    git(data, &["commit", "-m", "Publish author note"]);
    identifier
}

#[test]
fn memory_sync_cli_refresh_with_notes_accepts_different_host_aliases() {
    let (owned, marker, author_home) = setup();
    let peer_home = owned.path().join("alias-peer");
    task_sync_opt_in::enable(&peer_home, "memory-cli-fixture");
    success(command(&peer_home, &marker).arg("init").output().unwrap());
    let folder = data_worktree::folder(&marker);
    let data = folder.parent().unwrap();
    let identifier = publish_noted_author(&author_home, &marker, &folder);
    let author_basis = owned.path().join("author-alias-project.json");
    fs::copy(folder.join("project.json"), &author_basis).unwrap();
    success(sync_output(
        &peer_home,
        &marker,
        "import",
        &folder,
        &["--include-memories", "--empty-basis", "--json"],
    ));
    work(
        &peer_home,
        &marker,
        &["update", &identifier, "--title", "peer changed task"],
    );
    let peer_flags = [
        "--include-memories",
        "--include-notes",
        "--origin-machine",
        "host-b",
        "--json",
    ];
    success(sync_output(
        &peer_home,
        &marker,
        "export",
        &folder,
        &peer_flags,
    ));
    let refreshed: Value =
        serde_json::from_slice(&fs::read(folder.join("project.json")).unwrap()).unwrap();
    assert_eq!(refreshed["profile"]["origin_machine"], "host-b");
    assert_eq!(refreshed["profile"]["include_notes"], true);
    git(data, &["add", ".engram"]);
    git(data, &["commit", "-m", "Publish second alias refresh"]);
    success(sync_output(
        &author_home,
        &marker,
        "import",
        &folder,
        &[
            "--include-memories",
            "--basis",
            author_basis.to_str().unwrap(),
            "--json",
        ],
    ));
    let observed = engram::task_sync::data_worktree::DataWorktree::observe(
        marker.parent().unwrap(),
        data,
        "engram-data",
    )
    .unwrap();
    let documents = directory::read_resolved_data_folder_with_memories(&observed).unwrap();
    assert_eq!(documents.tree.tasks[0].title, "peer changed task");
    assert_eq!(documents.tree.tasks[0].notes.len(), 1);
}

#[test]
fn memory_sync_cli_refresh_stale_intent_restore_keeps_new_store_change() {
    let (owned, marker, home) = setup();
    work(&home, &marker, &["add", "initial task"]);
    let folder = data_worktree::folder(&marker);
    let data = folder.parent().unwrap();
    let flags = ["--include-memories", "--json"];
    success(sync_output(&home, &marker, "export", &folder, &flags));
    git(data, &["add", ".engram"]);
    git(
        data,
        &["commit", "-m", "Publish initial empty memory component"],
    );
    let retained = owned.path().join("retained-refresh");
    copy_tree(&folder, &retained);
    let task = fs::read_dir(folder.join("tasks"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let identifier = task.file_stem().unwrap().to_str().unwrap();
    work(
        &home,
        &marker,
        &["update", identifier, "--title", "interrupted intent"],
    );
    success(sync_output(&home, &marker, "export", &folder, &flags));
    // Model a stopped process immediately before it removes the full fence.
    let fence = folder.join(format!("project-{}.pending", uuid::Uuid::new_v4()));
    fs::copy(folder.join("project.json"), &fence).unwrap();
    work(
        &home,
        &marker,
        &["update", identifier, "--title", "new store change"],
    );
    let before = files(&folder);
    let ordinary = sync_output(&home, &marker, "export", &folder, &flags);
    assert_eq!(
        serde_json::from_slice::<Value>(&ordinary.stderr).unwrap()["error"]["code"],
        "memory_task_export_recovery_required"
    );
    let recovery = sync_output(
        &home,
        &marker,
        "export",
        &folder,
        &[
            "--include-memories",
            "--memory-recovery-basis",
            retained.to_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&recovery.stderr).unwrap()["error"]["code"],
        "memory_task_export_recovery_mismatch"
    );
    assert!(
        String::from_utf8_lossy(&recovery.stderr)
            .contains("git restore --source=HEAD --worktree -- .engram")
    );
    assert_eq!(files(&folder), before);
    git(
        data,
        &["restore", "--source=HEAD", "--worktree", "--", ".engram"],
    );
    fs::remove_file(fence).unwrap();
    success(sync_output(&home, &marker, "export", &folder, &flags));
    let published: Value = serde_json::from_slice(&fs::read(task).unwrap()).unwrap();
    assert_eq!(published["title"], "new store change");
}

#[test]
fn memory_sync_cli_refusal_leaves_tasks_and_capability_untouched() {
    let (_owned, marker, home) = setup();
    let folder = data_worktree::folder(&marker);
    success(sync_output(&home, &marker, "export", &folder, &["--json"]));
    fs::write(
        folder.join(".gitignore"),
        "/*\n!/.gitignore\n!/project.json\n!/tasks/\n",
    )
    .unwrap();
    let data = folder.parent().unwrap();
    git(data, &["add", ".engram"]);
    git(data, &["commit", "-m", "Record original task-only policy"]);
    work(&home, &marker, &["add", "unexported task"]);
    work(
        &home,
        &marker,
        &["remember", "unexported rule", "--key", "rule"],
    );
    let before = files(&folder);
    let refused = sync_output(
        &home,
        &marker,
        "export",
        &folder,
        &["--include-memories", "--json"],
    );
    assert!(!refused.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&refused.stderr).unwrap()["error"]["code"],
        "memory_ignore_policy_excludes_tree"
    );
    assert_eq!(files(&folder), before);
}

#[test]
fn memory_sync_cli_import_reports_refusals_and_requires_explicit_seed_override() {
    use engram::task_sync::memories::{self, ImportOptions, SeedManifestEntry};
    let (owned, marker, source) = setup();
    let target = owned.path().join("second-home");
    task_sync_opt_in::enable(&target, "memory-cli-fixture");
    success(command(&target, &marker).arg("init").output().unwrap());
    for home in [&source, &target] {
        work(home, &marker, &["remember", "seed body", "--key", "seed"]);
    }
    work(
        &source,
        &marker,
        &["remember", "shared", "--key", "conflict"],
    );
    work(
        &target,
        &marker,
        &["remember", "local edit", "--key", "conflict"],
    );
    let folder = data_worktree::folder(&marker);
    success(sync_output(
        &source,
        &marker,
        "export",
        &folder,
        &["--include-memories", "--json"],
    ));
    git(folder.parent().unwrap(), &["add", ".engram"]);
    git(
        folder.parent().unwrap(),
        &["commit", "-m", "Record shared memories"],
    );
    let refused = sync_output(
        &target,
        &marker,
        "import",
        &folder,
        &["--empty-basis", "--json"],
    );
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(
        serde_json::from_slice::<Value>(&refused.stderr).unwrap()["error"]["code"],
        "memory_profile_requires_opt_in"
    );
    let project = ProjectId("memory-cli-fixture".into());
    let store = SqliteStore::open(project_database_path(&target, &project)).unwrap();
    let local = store
        .project_memory_sync_head(&project, "seed")
        .unwrap()
        .unwrap();
    drop(store);
    let source_store = SqliteStore::open(project_database_path(&source, &project)).unwrap();
    let shared = source_store
        .project_memory_sync_head(&project, "seed")
        .unwrap()
        .unwrap();
    drop(source_store);
    let options = std::collections::BTreeMap::from([(
        "seed".to_owned(),
        ImportOptions {
            seed: Some(SeedManifestEntry {
                project_id: project,
                key: "seed".into(),
                local_version_id: local,
                shared_revision_id: shared,
                body_sha256: memories::body_sha256("seed body"),
            }),
            // The CLI must override this serialized value unless explicitly granted.
            mark_untagged_seed: true,
            ..ImportOptions::default()
        },
    )]);
    let resolution = owned.path().join("resolution.json");
    fs::write(&resolution, serde_json::to_vec(&options).unwrap()).unwrap();
    let args = [
        "--include-memories",
        "--empty-basis",
        "--json",
        "--memory-resolution",
        resolution.to_str().unwrap(),
    ];
    let first = sync_output(&target, &marker, "import", &folder, &args);
    assert_eq!(first.status.code(), Some(3));
    let receipt: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(receipt["memory_partial"], true);
    assert!(
        receipt["memories"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["key"] == "seed" && entry["status"] == "refused")
    );
    let mut override_args = args.to_vec();
    override_args.push("--mark-untagged-seeds");
    let second = sync_output(&target, &marker, "import", &folder, &override_args);
    assert_eq!(second.status.code(), Some(3));
    let receipt: Value = serde_json::from_slice(&second.stdout).unwrap();
    assert!(
        receipt["memories"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["key"] == "conflict" && entry["status"] == "refused")
    );
    assert!(
        receipt["memories"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["key"] == "seed"
                && entry["status"] == "applied"
                && entry["outcome"]["appended_revisions"] == 1)
    );
}

#[test]
fn memory_sync_cli_concurrent_publication_refuses_before_task_writes() {
    let (_owned, marker, home) = setup();
    let folder = data_worktree::folder(&marker);
    success(sync_output(&home, &marker, "export", &folder, &["--json"]));
    work(&home, &marker, &["add", "task awaiting export"]);
    let observed = engram::task_sync::data_worktree::DataWorktree::observe(
        marker.parent().unwrap(),
        folder.parent().unwrap(),
        "engram-data",
    )
    .unwrap();
    let held = engram::task_sync::memories::directory::publication_lock(&observed).unwrap();
    let before = files(&folder);
    let refused = sync_output(
        &home,
        &marker,
        "export",
        &folder,
        &["--include-memories", "--json"],
    );
    assert!(!refused.status.success());
    assert_eq!(files(&folder), before);
    drop(held);
}
