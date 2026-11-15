use super::*;

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
    let mut result = Default::default();
    collect(root, root, &mut result);
    result
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
    data_worktree::clear(&marker);
    success(sync_output(&home, &marker, "export", &folder, &["--json"]));
    copy_tree(&retained.join("memories"), &folder.join("memories"));
    let project = folder.join("project.json");
    let current_tasks = fs::read(&project).unwrap();
    // Capture the exact bytes produced by this CLI, then simulate interruption
    // while that same capability envelope is being staged before its rename.
    success(sync_output(
        &home,
        &marker,
        "export",
        &folder,
        &["--include-memories", "--json"],
    ));
    let intended = fs::read(&project).unwrap();
    fs::write(&project, &current_tasks).unwrap();
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
            ..Default::default()
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
    let mut marked = args.to_vec();
    marked.push("--mark-untagged-seeds");
    let second = sync_output(&target, &marker, "import", &folder, &marked);
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
