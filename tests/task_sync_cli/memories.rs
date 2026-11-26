use super::*;
use engram::task_sync::{
    data_worktree::DataWorktree,
    directory::{ExportParent, FolderWrite},
};
use std::collections::BTreeMap;

const OWNED_JOURNAL_PREFIX: &str = "engram-task-sync-folder-write-";

fn success(output: Output) -> Output {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn assert_combined_rebuild(output: &Output) {
    assert!(!output.status.success());
    let refusal = refusal_json(output);
    let error = &refusal["error"];
    assert_eq!(error["code"], "task_sync_incremental_basis_unavailable");
    let reason = error["details"]["reason"].as_str().unwrap();
    assert!(
        reason.contains("export --rebuild --include-memories --memory-recovery-basis"),
        "{reason}"
    );
    assert!(!reason.contains("commit the exported folder"), "{reason}");
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

/// How many Git processes one `--include-memories` export starts, read from
/// Git's own trace, once `keys` memory keys are committed and one of them has
/// a new revision since.
fn export_git_starts(keys: usize) -> usize {
    let (owned, marker, home) = setup();
    for n in 0..keys {
        let key = format!("rule-{n}");
        work(
            &home,
            &marker,
            &["remember", &format!("rule {n}"), "--key", &key],
        );
    }
    let folder = data_worktree::folder(&marker);
    let data = folder.parent().unwrap();
    let export = ["--include-memories", "--json"];
    success(sync_output(&home, &marker, "export", &folder, &export));
    git(data, &["add", ".engram"]);
    git(data, &["commit", "-m", "Record the memory keys"]);
    work(
        &home,
        &marker,
        &["remember", "rule 0 revised", "--key", "rule-0", "--revise"],
    );
    let trace = owned.path().join("export-git-trace.json");
    success(
        command(&home, &marker)
            .env("GIT_TRACE2_EVENT", &trace)
            .args([
                "task-sync",
                "--actor-id",
                "operator",
                "--session-id",
                "sync-operator",
                "export",
            ])
            .arg(&folder)
            .args(export)
            .output()
            .unwrap(),
    );
    let starts = fs::read_to_string(&trace)
        .unwrap()
        .lines()
        .filter(|line| line.contains(r#""event":"start""#))
        .count();
    assert!(starts > 0, "Git's trace recorded no process start");
    starts
}

#[test]
fn memory_sync_cli_export_starts_as_many_git_processes_for_forty_keys_as_for_two() {
    let few = export_git_starts(2);
    let many = export_git_starts(40);
    assert_eq!(
        many, few,
        "an export over 40 committed memory keys started {many} Git processes, over 2 keys {few}"
    );
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
    // Advance the task comparison basis to the peer commit. The memory
    // retirement still needs its own explicit decision.
    let applied = sync_output(&author_home, &marker, "import", &folder, &import_args);
    assert_eq!(applied.status.code(), Some(3), "{applied:?}");
    let receipt: Value = serde_json::from_slice(&applied.stdout).unwrap();
    assert_eq!(
        receipt["memories"][0]["status"],
        "memory_concurrent_retirement"
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
fn memory_sync_cli_invalid_committed_envelope_refuses_before_export_writes() {
    for missing in [true, false] {
        let (owned, marker, home) = setup();
        let peer_home = owned.path().join("peer-home");
        task_sync_opt_in::enable(&peer_home, "memory-cli-fixture");
        success(command(&peer_home, &marker).arg("init").output().unwrap());
        work(
            &peer_home,
            &marker,
            &["remember", "peer-only history", "--key", "peer"],
        );
        let folder = data_worktree::folder(&marker);
        let data = folder.parent().unwrap();
        success(sync_output(
            &peer_home,
            &marker,
            "export",
            &folder,
            &["--include-memories", "--json"],
        ));
        let valid = fs::read(folder.join("project.json")).unwrap();
        let peer_files = files(&folder.join("memories"));
        git(data, &["add", ".engram"]);
        git(data, &["commit", "-m", "Record peer-only memory"]);
        work(
            &home,
            &marker,
            &["remember", "local-only history", "--key", "local"],
        );
        if missing {
            fs::remove_file(folder.join("project.json")).unwrap();
        } else {
            fs::write(folder.join("project.json"), "malformed envelope").unwrap();
        }
        git(data, &["add", ".engram"]);
        git(data, &["commit", "-m", "Record invalid project envelope"]);
        let before = files(&folder);
        let saved = owned.path().join("saved-component");
        fs::rename(&folder, &saved).unwrap();
        for args in [vec!["--include-memories", "--json"], vec!["--json"]] {
            let refused = sync_output(&home, &marker, "export", &folder, &args);
            assert_eq!(
                refused.status.code(),
                Some(1),
                "{}",
                String::from_utf8_lossy(&refused.stderr)
            );
            let error = refusal_json(&refused);
            assert_eq!(error["error"]["code"], "memory_export_basis_invalid");
            assert!(
                !folder.exists(),
                "refusal must not recreate any shared export file"
            );
            assert_eq!(files(&saved), before);
        }
        fs::rename(&saved, &folder).unwrap();
        fs::write(folder.join("project.json"), &valid).unwrap();
        git(data, &["add", ".engram"]);
        git(
            data,
            &["commit", "-m", "Restore matching committed envelope"],
        );
        success(sync_output(
            &home,
            &marker,
            "export",
            &folder,
            &["--include-memories", "--json"],
        ));
        let after = files(&folder.join("memories"));
        for (path, bytes) in peer_files {
            assert_eq!(
                after.get(&path),
                Some(&bytes),
                "peer revision {path:?} must survive"
            );
        }
        assert!(folder.join("memories/local/head.json").is_file());
        assert!(folder.join("memories/peer/head.json").is_file());
    }
}

#[test]
fn memory_sync_cli_withholds_a_key_whose_target_a_held_removal_keeps_out_of_the_folder() {
    let (owned, marker, home) = setup();
    let folder = data_worktree::folder(&marker);
    let data = folder.parent().unwrap();
    work(&home, &marker, &["add", "Retiring target"]);
    success(sync_output(
        &home,
        &marker,
        "export",
        &folder,
        &["--include-memories", "--json"],
    ));
    // The folder carries the memory component, which the task-only folder
    // reader refuses, so the one task's id is read from its file name.
    let target = fs::read_dir(folder.join("tasks"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .and_then(|path| path.file_stem()?.to_str().map(str::to_owned))
        .unwrap();
    let basis = owned.path().join("own-basis.json");
    fs::copy(folder.join("project.json"), &basis).unwrap();
    git(data, &["add", ".engram"]);
    git(data, &["commit", "-m", "Publish the target"]);
    success(sync_output(
        &home,
        &marker,
        "import",
        &folder,
        &[
            "--include-memories",
            "--basis",
            basis.to_str().unwrap(),
            "--json",
        ],
    ));
    work(
        &home,
        &marker,
        &[
            "remember",
            "retires with the target",
            "--key",
            "rule",
            "--retires-with",
            &format!("local:{target}"),
        ],
    );
    // Another host removes the target; this host created it, so the removal
    // is refused and held, and the folder keeps it out.
    fs::remove_file(folder.join("tasks").join(format!("{target}.json"))).unwrap();
    git(data, &["add", "-A", ".engram"]);
    git(data, &["commit", "-m", "Remove the target elsewhere"]);
    let held = sync_output(
        &home,
        &marker,
        "import",
        &folder,
        &["--include-memories", "--json"],
    );
    assert_eq!(
        held.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&held.stderr)
    );
    // The key whose target the folder does not hold is withheld whole.
    let exported = success(sync_output(
        &home,
        &marker,
        "export",
        &folder,
        &["--include-memories", "--json"],
    ));
    assert!(
        String::from_utf8_lossy(&exported.stderr).contains("Memory key rule withheld:"),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    assert!(!folder.join("tasks").join(format!("{target}.json")).exists());
}

#[test]
fn memory_sync_cli_refuses_a_published_key_whose_target_a_held_removal_keeps_out() {
    let (owned, marker, home) = setup();
    let folder = data_worktree::folder(&marker);
    let data = folder.parent().unwrap();
    work(&home, &marker, &["add", "Retiring target"]);
    success(sync_output(
        &home,
        &marker,
        "export",
        &folder,
        &["--include-memories", "--json"],
    ));
    let target = fs::read_dir(folder.join("tasks"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .and_then(|path| path.file_stem()?.to_str().map(str::to_owned))
        .unwrap();
    let basis = owned.path().join("own-basis.json");
    fs::copy(folder.join("project.json"), &basis).unwrap();
    git(data, &["add", ".engram"]);
    git(data, &["commit", "-m", "Publish the target"]);
    success(sync_output(
        &home,
        &marker,
        "import",
        &folder,
        &[
            "--include-memories",
            "--basis",
            basis.to_str().unwrap(),
            "--json",
        ],
    ));
    work(
        &home,
        &marker,
        &[
            "remember",
            "retires with the target",
            "--key",
            "rule",
            "--retires-with",
            &format!("local:{target}"),
        ],
    );
    // The key is published while the folder still holds its target.
    success(sync_output(
        &home,
        &marker,
        "export",
        &folder,
        &["--include-memories", "--json"],
    ));
    git(data, &["add", "-A", ".engram"]);
    git(data, &["commit", "-m", "Publish the key"]);
    // Another host removes the target and keeps the key; the removal is held.
    fs::remove_file(folder.join("tasks").join(format!("{target}.json"))).unwrap();
    git(data, &["add", "-A", ".engram"]);
    git(data, &["commit", "-m", "Remove the target elsewhere"]);
    let held = sync_output(
        &home,
        &marker,
        "import",
        &folder,
        &["--include-memories", "--json"],
    );
    assert_eq!(
        held.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&held.stderr)
    );
    // The folder already holds the key, so withholding it could not keep the
    // missing target out of view: the export refuses before it writes.
    let before = files(&folder);
    let refused = sync_output(
        &home,
        &marker,
        "export",
        &folder,
        &["--include-memories", "--json"],
    );
    assert_eq!(
        refused.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    let error: Value = serde_json::from_slice(&refused.stderr).unwrap();
    assert_eq!(error["error"]["code"], "task_sync_removed_planning");
    let message = error["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("rule") && message.contains(&target),
        "{message}"
    );
    assert_eq!(files(&folder), before);
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
        refusal_json(&result)["error"]["code"],
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
    work(&home, &marker, &["add", "second task"]);
    work(
        &home,
        &marker,
        &["remember", "new shared rule", "--key", "new-rule"],
    );
    // Export the new task, capturing the exact capability envelope this CLI
    // writes. Then model a capability publication interrupted while staging
    // that envelope: the current envelope without the capability, and half
    // of the staged bytes. The export added a memory key, so committing it
    // would make the retained pre-export memory basis stale.
    success(sync_output(
        &home,
        &marker,
        "export",
        &folder,
        &["--include-memories", "--json"],
    ));
    let memory_before = files(&folder.join("memories"));
    assert!(folder.join("memories/new-rule/head.json").is_file());
    let project = folder.join("project.json");
    let intended = fs::read(&project).unwrap();
    let mut current_tasks = serde_json::from_slice::<Value>(&intended).unwrap();
    assert!(
        current_tasks
            .as_object_mut()
            .unwrap()
            .remove("memory_profile")
            .is_some()
    );
    fs::write(&project, serde_json::to_vec_pretty(&current_tasks).unwrap()).unwrap();
    let pending = folder.join(format!("project-{}.pending", uuid::Uuid::new_v4()));
    fs::write(&pending, &intended[..intended.len() / 2]).unwrap();
    let args = [
        "--include-memories",
        "--memory-recovery-basis",
        retained.to_str().unwrap(),
        "--json",
    ];
    let before = files(&folder);
    assert_combined_rebuild(&sync_output(&home, &marker, "export", &folder, &args));
    assert_eq!(files(&folder), before);
    let mut rebuild_args = args.to_vec();
    rebuild_args.push("--rebuild");
    let recovered = success(sync_output(
        &home,
        &marker,
        "export",
        &folder,
        &rebuild_args,
    ));
    let receipt: Value = serde_json::from_slice(&recovered.stdout).unwrap();
    assert_eq!(receipt["route"], "rebuild");
    assert!(!pending.exists());
    let envelope = serde_json::from_slice::<Value>(&fs::read(&project).unwrap()).unwrap();
    assert!(envelope.get("memory_profile").is_some(), "{envelope}");
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
}

#[test]
fn memory_sync_cli_capability_recovery_precedes_a_later_task_change() {
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
    work(&home, &marker, &["add", "second task"]);
    success(sync_output(
        &home,
        &marker,
        "export",
        &folder,
        &["--include-memories", "--json"],
    ));
    // A capability publication interrupted while staging the envelope of
    // the second export, as in the recovery test above.
    let project = folder.join("project.json");
    let staged = fs::read(&project).unwrap();
    let mut without = serde_json::from_slice::<Value>(&staged).unwrap();
    without.as_object_mut().unwrap().remove("memory_profile");
    fs::write(&project, serde_json::to_vec_pretty(&without).unwrap()).unwrap();
    let pending = folder.join(format!("project-{}.pending", uuid::Uuid::new_v4()));
    fs::write(&pending, &staged[..staged.len() / 2]).unwrap();
    // The store changes before the retry, so an explicit rebuild publishes a
    // newer envelope than the one staged.
    work(&home, &marker, &["add", "third task"]);
    let args = [
        "--include-memories",
        "--memory-recovery-basis",
        retained.to_str().unwrap(),
        "--json",
    ];
    let before = files(&folder);
    assert_combined_rebuild(&sync_output(&home, &marker, "export", &folder, &args));
    assert_eq!(files(&folder), before);
    let mut rebuild_args = args.to_vec();
    rebuild_args.push("--rebuild");
    let rebuilt = success(sync_output(
        &home,
        &marker,
        "export",
        &folder,
        &rebuild_args,
    ));
    let receipt: Value = serde_json::from_slice(&rebuilt.stdout).unwrap();
    assert_eq!(receipt["route"], "rebuild");
    assert!(!pending.exists());
    let envelope = serde_json::from_slice::<Value>(&fs::read(&project).unwrap()).unwrap();
    assert!(envelope.get("memory_profile").is_some(), "{envelope}");
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
        3
    );
}

fn confirmed_memory_folder(home: &Path, marker: &Path) -> std::path::PathBuf {
    work(home, marker, &["add", "first task"]);
    work(home, marker, &["remember", "shared rule", "--key", "rule"]);
    let folder = data_worktree::folder(marker);
    let data = folder.parent().unwrap();
    let args = ["--include-memories", "--json"];
    success(sync_output(home, marker, "export", &folder, &args));
    git(data, &["add", ".engram"]);
    git(data, &["commit", "-m", "Publish task and memory"]);
    success(sync_output(home, marker, "export", &folder, &args));
    folder
}

#[test]
fn memory_sync_cli_rebuilds_when_loose_head_staging_blocks_exact_confirmation() {
    let (owned, marker, home) = setup();
    let folder = confirmed_memory_folder(&home, &marker);
    let data = folder.parent().unwrap();
    let retained = owned.path().join("retained-memory-basis");
    copy_tree(&folder, &retained);
    let memory_before = files(&folder.join("memories"));
    work(&home, &marker, &["add", "second task"]);
    success(sync_output(
        &home,
        &marker,
        "export",
        &folder,
        &["--include-memories", "--json"],
    ));
    let key = folder.join("memories/rule");
    let head = fs::read(key.join("head.json")).unwrap();
    let pending = key.join(format!("head-{}.pending", uuid::Uuid::new_v4()));
    fs::write(&pending, &head[..head.len() / 2]).unwrap();
    git(
        data,
        &["add", "--", ".engram/project.json", ".engram/tasks"],
    );
    git(data, &["commit", "-m", "Commit the task output alone"]);
    let mut args = vec![
        "--include-memories",
        "--memory-recovery-basis",
        retained.to_str().unwrap(),
        "--json",
    ];
    let before = files(&folder);
    let refused = sync_output(&home, &marker, "export", &folder, &args);
    assert_combined_rebuild(&refused);
    assert!(String::from_utf8_lossy(&refused.stderr).contains("restore the original basis"));
    assert_eq!(files(&folder), before);
    git(data, &["branch", "preserve-exact-task-output"]);
    git(data, &["reset", "--mixed", "HEAD^"]);
    assert_eq!(files(&folder), before);
    args.push("--rebuild");
    let recovered = success(sync_output(&home, &marker, "export", &folder, &args));
    let receipt: Value = serde_json::from_slice(&recovered.stdout).unwrap();
    assert_eq!(receipt["route"], "rebuild");
    assert_eq!(receipt["task_count"], 2);
    assert!(!pending.exists());
    assert_eq!(files(&folder.join("memories")), memory_before);
}

#[test]
fn memory_sync_cli_ignored_staging_refusal_preserves_exact_proof() {
    let (owned, marker, home) = setup();
    let folder = confirmed_memory_folder(&home, &marker);
    let checkout = folder.parent().unwrap();
    let data = combined_data_worktree(&marker, checkout);
    let parent = data.head_commit().unwrap();
    let retained = owned.path().join("retained-memory-basis");
    copy_tree(&folder, &retained);
    work(&home, &marker, &["add", "second task"]);
    work(
        &home,
        &marker,
        &["remember", "another shared rule", "--key", "extra"],
    );
    success(sync_output(
        &home,
        &marker,
        "export",
        &folder,
        &["--include-memories", "--json"],
    ));
    git(checkout, &["add", ".engram"]);
    git(checkout, &["commit", "-m", "Publish exact planning"]);
    let excludes = owned.path().join("recovery-excludes");
    fs::write(&excludes, "/.engram/memories/rule/*.pending\n").unwrap();
    git(
        checkout,
        &["config", "core.excludesFile", excludes.to_str().unwrap()],
    );
    let memory = folder.join("memories/rule");
    let canonical = fs::read(memory.join("head.json")).unwrap();
    let pending_name = format!("head-{}.pending", uuid::Uuid::new_v4());
    let pending = memory.join(&pending_name);
    fs::write(&pending, b"unrelated staging").unwrap();
    let relative = format!(".engram/memories/rule/{pending_name}");
    git(checkout, &["check-ignore", "--", &relative]);

    let journal = combined_export_journal(&marker, checkout);
    let head = combined_folder_head(&home);
    let proof = fs::read(&journal).unwrap();
    let before = files(&folder);
    let mut args = vec![
        "--include-memories",
        "--memory-recovery-basis",
        retained.to_str().unwrap(),
        "--json",
    ];
    let refused = sync_output(&home, &marker, "export", &folder, &args);
    assert_combined_rebuild(&refused);
    assert!(String::from_utf8_lossy(&refused.stderr).contains("restore the original basis"));
    assert!(String::from_utf8_lossy(&refused.stderr).contains(parent));
    assert_eq!(files(&folder), before);
    assert_eq!(combined_folder_head(&home), head);
    assert_eq!(fs::read(&journal).unwrap(), proof);

    // Retain the inconsistent staging separately before restoring the parent.
    fs::rename(&pending, owned.path().join("preserved-pending")).unwrap();
    fs::write(&pending, &canonical[..canonical.len() / 2]).unwrap();
    args.push("--rebuild");
    let before = files(&folder);
    let refused = sync_output(&home, &marker, "export", &folder, &args);
    assert_combined_rebuild(&refused);
    assert!(String::from_utf8_lossy(&refused.stderr).contains(parent));
    assert_eq!(files(&folder), before);
    assert_eq!(combined_folder_head(&home), head);
    assert_eq!(fs::read(&journal).unwrap(), proof);

    git(checkout, &["branch", "preserve-exact-planning"]);
    git(checkout, &["reset", "--mixed", "HEAD^"]);
    let recovered = success(sync_output(&home, &marker, "export", &folder, &args));
    let receipt: Value = serde_json::from_slice(&recovered.stdout).unwrap();
    assert_eq!(receipt["route"], "rebuild");
    assert_eq!(receipt["task_count"], 2);
    assert!(!pending.exists());
    assert!(folder.join("memories/extra/head.json").is_file());
    assert_ne!(combined_folder_head(&home), head);
    assert_ne!(fs::read(journal).unwrap(), proof);
}

#[test]
fn memory_sync_cli_discarded_combined_write_with_capability_staging_names_recovery() {
    let (owned, marker, home) = setup();
    let folder = confirmed_memory_folder(&home, &marker);
    let data = folder.parent().unwrap();
    let retained = owned.path().join("retained-memory-basis");
    copy_tree(&folder, &retained);
    work(&home, &marker, &["add", "second task"]);
    success(sync_output(
        &home,
        &marker,
        "export",
        &folder,
        &["--include-memories", "--json"],
    ));
    git(
        data,
        &["restore", "--worktree", "--", ".engram/project.json"],
    );
    git(data, &["clean", "-f", "--", ".engram/tasks"]);
    let intended = fs::read(folder.join("project.json")).unwrap();
    let pending = folder.join(format!("project-{}.pending", uuid::Uuid::new_v4()));
    fs::write(&pending, &intended[..intended.len() / 2]).unwrap();
    let mut args = vec![
        "--include-memories",
        "--memory-recovery-basis",
        retained.to_str().unwrap(),
        "--json",
    ];
    let before = files(&folder);
    let refused = sync_output(&home, &marker, "export", &folder, &args);
    assert_combined_rebuild(&refused);
    assert!(!String::from_utf8_lossy(&refused.stderr).contains("task-sync import"));
    assert_eq!(files(&folder), before);
    args.push("--rebuild");
    let recovered = success(sync_output(&home, &marker, "export", &folder, &args));
    let receipt: Value = serde_json::from_slice(&recovered.stdout).unwrap();
    assert_eq!(receipt["route"], "rebuild");
    assert_eq!(receipt["task_count"], 2);
    assert!(!pending.exists());
}

#[test]
fn memory_sync_cli_moved_tip_refuses_before_capability_cleanup() {
    let (owned, marker, home) = setup();
    let folder = confirmed_memory_folder(&home, &marker);
    let data = folder.parent().unwrap();
    let retained = owned.path().join("retained-memory-basis");
    copy_tree(&folder, &retained);
    let original = files(&folder);
    let intended = fs::read(folder.join("project.json")).unwrap();
    let pending = folder.join(format!("project-{}.pending", uuid::Uuid::new_v4()));
    fs::write(&pending, &intended[..intended.len() / 2]).unwrap();
    git(
        data,
        &[
            "commit",
            "--allow-empty",
            "-m",
            "Move past the admitted tip",
        ],
    );
    let args = [
        "--include-memories",
        "--memory-recovery-basis",
        retained.to_str().unwrap(),
        "--json",
    ];
    let before = files(&folder);
    let refused = sync_output(&home, &marker, "export", &folder, &args);
    assert!(!refused.status.success());
    let refusal = refusal_json(&refused);
    assert_eq!(
        refusal["error"]["code"],
        "task_sync_incremental_basis_unavailable"
    );
    let reason = refusal["error"]["details"]["reason"].as_str().unwrap();
    assert!(reason.contains("restore the admitted tip"), "{reason}");
    assert!(reason.contains("--memory-recovery-basis"), "{reason}");
    assert!(!reason.contains("task-sync import"), "{reason}");
    assert_eq!(files(&folder), before);
    git(data, &["reset", "--keep", "HEAD^"]);
    let recovered = success(sync_output(&home, &marker, "export", &folder, &args));
    let receipt: Value = serde_json::from_slice(&recovered.stdout).unwrap();
    assert_eq!(receipt["route"], "unchanged");
    assert_eq!(files(&folder), original);
}

#[test]
fn memory_sync_cli_explicit_recovery_restores_a_missing_retained_revision() {
    let (owned, marker, home) = setup();
    work(&home, &marker, &["add", "shared task"]);
    work(
        &home,
        &marker,
        &["remember", "shared rule", "--key", "rule"],
    );
    let folder = data_worktree::folder(&marker);
    let data = folder.parent().unwrap();
    let combined = ["--include-memories", "--json"];
    success(sync_output(&home, &marker, "export", &folder, &combined));
    git(data, &["add", ".engram"]);
    git(
        data,
        &["commit", "-m", "Publish task planning and memories"],
    );
    success(sync_output(&home, &marker, "export", &folder, &combined));
    let retained = owned.path().join("retained-memory-basis");
    copy_tree(&folder, &retained);
    let expected = files(&folder);
    let revision = fs::read_dir(folder.join("memories").join("rule"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.file_name().unwrap() != "head.json")
        .unwrap();
    fs::remove_file(&revision).unwrap();

    let before = files(&folder);
    assert!(
        !sync_output(&home, &marker, "export", &folder, &combined)
            .status
            .success()
    );
    assert_eq!(files(&folder), before);
    let recovered = success(sync_output(
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
    ));
    let receipt: Value = serde_json::from_slice(&recovered.stdout).unwrap();
    assert_eq!(receipt["route"], "unchanged");
    assert_eq!(files(&folder), expected);
    git(data, &["diff", "--exit-code", "HEAD", "--", ".engram"]);
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
    assert_combined_rebuild(&refused);
    assert_eq!(files(&folder), before);
}

#[test]
fn memory_sync_cli_revision_file_with_other_bytes_refuses_before_task_writes() {
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
    let data = folder.parent().unwrap();
    git(data, &["add", ".engram"]);
    git(data, &["commit", "-m", "Record shared memories"]);
    // A checkout that converted the revision to CRLF: the same document,
    // other bytes.
    let revision = fs::read_dir(folder.join("memories").join("rule"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.file_name().unwrap() != "head.json")
        .unwrap();
    let converted = String::from_utf8(fs::read(&revision).unwrap())
        .unwrap()
        .replace('\n', "\r\n");
    fs::write(&revision, converted).unwrap();
    work(&home, &marker, &["add", "task awaiting export"]);
    let before = files(&folder);
    let refused = sync_output(
        &home,
        &marker,
        "export",
        &folder,
        &["--include-memories", "--json"],
    );
    assert!(!refused.status.success());
    let error = serde_json::from_slice::<Value>(&refused.stderr).unwrap();
    assert_eq!(error["error"]["code"], "memory_export_component_changed");
    assert!(error.to_string().contains("CRLF"), "{error}");
    assert_eq!(files(&folder), before, "no task file was written");
}

#[test]
fn memory_sync_cli_incremental_import_applies_new_memories() {
    let (owned, marker, source) = setup();
    let target = owned.path().join("second-home");
    task_sync_opt_in::enable(&target, "memory-cli-fixture");
    success(command(&target, &marker).arg("init").output().unwrap());
    let folder = data_worktree::folder(&marker);
    let data = folder.parent().unwrap();
    let export_and_commit = |message: &str| {
        success(sync_output(
            &source,
            &marker,
            "export",
            &folder,
            &["--include-memories", "--json"],
        ));
        git(data, &["add", ".engram"]);
        git(data, &["commit", "-m", message]);
    };
    work(
        &source,
        &marker,
        &["remember", "first rule", "--key", "first"],
    );
    work(&source, &marker, &["add", "first task"]);
    export_and_commit("Record the first exchange");
    success(sync_output(
        &target,
        &marker,
        "import",
        &folder,
        &["--include-memories", "--empty-basis", "--json"],
    ));
    work(
        &source,
        &marker,
        &["remember", "second rule", "--key", "second"],
    );
    work(&source, &marker, &["add", "second task"]);
    export_and_commit("Record the second exchange");
    let imported = success(sync_output(
        &target,
        &marker,
        "import",
        &folder,
        &["--include-memories", "--json"],
    ));
    let receipt: Value = serde_json::from_slice(&imported.stdout).unwrap();
    assert_eq!(receipt["route"], "incremental", "{receipt}");
    assert!(
        receipt["memories"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["key"] == "second" && entry["status"] == "applied"),
        "{receipt}"
    );
    let project = ProjectId("memory-cli-fixture".into());
    let store = SqliteStore::open(project_database_path(&target, &project)).unwrap();
    assert!(
        store
            .project_memory_sync_head(&project, "second")
            .unwrap()
            .is_some()
    );
}

#[test]
fn memory_sync_cli_previews_its_own_combined_commit_without_consuming_the_proof() {
    let (owned, marker, home) = setup();
    let folder = data_worktree::folder(&marker);
    let checkout = folder.parent().unwrap();
    work(
        &home,
        &marker,
        &["remember", "shared rule", "--key", "rule"],
    );
    work(&home, &marker, &["add", "shared task"]);
    success(sync_output(
        &home,
        &marker,
        "export",
        &folder,
        &["--include-memories", "--json"],
    ));
    git(checkout, &["add", ".engram"]);
    git(checkout, &["commit", "-m", "Publish combined planning"]);

    let journal = combined_export_journal(&marker, checkout);
    let before_head = combined_folder_head(&home);
    let before_proof = fs::read(&journal).unwrap();
    let before_files = files(&folder);
    let resolution = owned.path().join("absent-memory-resolution.json");
    fs::write(&resolution, br#"{"absent":{}}"#).unwrap();
    for dry_run in [false, true] {
        let mut args = vec![
            "--include-memories",
            "--json",
            "--memory-resolution",
            resolution.to_str().unwrap(),
        ];
        if dry_run {
            args.push("--dry-run");
        }
        let refused = sync_output(&home, &marker, "import", &folder, &args);
        assert!(!refused.status.success());
        assert!(
            String::from_utf8_lossy(&refused.stderr)
                .contains("memory resolution names an absent selected key")
        );
        assert_eq!(files(&folder), before_files);
        assert_eq!(combined_folder_head(&home), before_head);
        assert_eq!(fs::read(&journal).unwrap(), before_proof);
    }
    let output = success(sync_output(
        &home,
        &marker,
        "import",
        &folder,
        &["--include-memories", "--json", "--dry-run"],
    ));
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["route"], "incremental", "{receipt}");
    assert!(
        receipt["memories"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["key"] == "rule"),
        "{receipt}"
    );
    assert_eq!(combined_folder_head(&home), before_head);
    assert_eq!(fs::read(&journal).unwrap(), before_proof);

    success(sync_output(
        &home,
        &marker,
        "import",
        &folder,
        &["--include-memories", "--json"],
    ));
    assert_ne!(combined_folder_head(&home), before_head);
    assert!(!journal.exists());
}

#[test]
fn memory_sync_cli_first_publication_recovery_requires_committed_memories() {
    let (owned, marker, home) = setup();
    let folder = data_worktree::folder(&marker);
    let checkout = folder.parent().unwrap();
    work(&home, &marker, &["add", "first task"]);
    work(
        &home,
        &marker,
        &["remember", "shared rule", "--key", "rule"],
    );
    success(sync_output(
        &home,
        &marker,
        "export",
        &folder,
        &["--include-memories", "--json"],
    ));
    let basis = owned.path().join("saved-project.json");
    fs::copy(folder.join("project.json"), &basis).unwrap();
    let journal = combined_export_journal(&marker, checkout);
    let slot_key = journal
        .file_stem()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(OWNED_JOURNAL_PREFIX))
        .unwrap()
        .parse()
        .unwrap();
    let data = combined_data_worktree(&marker, checkout);
    let mut unfinished = FolderWrite::pending(&data, Some(&slot_key))
        .unwrap()
        .unwrap();
    assert_eq!(unfinished.parent, Some(ExportParent::Unborn));
    assert!(unfinished.published);
    unfinished.published = false;
    unfinished.retain(&data).unwrap();
    let proof = fs::read(&journal).unwrap();
    git(
        checkout,
        &["add", "--", ".engram/project.json", ".engram/tasks"],
    );
    git(checkout, &["commit", "-m", "Commit only the task phase"]);
    assert!(folder.join("memories/rule/head.json").is_file());

    let before = files(&folder);
    let head = combined_folder_head(&home);
    let args = [
        "--include-memories",
        "--json",
        "--basis",
        basis.to_str().unwrap(),
    ];
    let refused = sync_output(&home, &marker, "import", &folder, &args);
    assert!(!refused.status.success());
    let reason = refusal_json(&refused);
    assert_eq!(reason["error"]["code"], "task_sync_commit_context_required");
    assert!(
        reason["error"]["details"]["reason"]
            .as_str()
            .unwrap()
            .contains("import --basis OWN_EXPORT/project.json --include-memories")
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("clean committed task and memory folder")
    );
    assert_eq!(files(&folder), before);
    assert_eq!(combined_folder_head(&home), head);
    assert_eq!(fs::read(&journal).unwrap(), proof);

    git(checkout, &["add", "--", ".engram/memories"]);
    git(
        checkout,
        &["commit", "-m", "Commit the complete memory component"],
    );
    success(sync_output(&home, &marker, "import", &folder, &args));
    assert_ne!(combined_folder_head(&home), head);
    assert!(!journal.exists());
    assert_eq!(files(&folder), before);
}

#[test]
fn memory_sync_cli_unbound_rebuild_names_combined_saved_basis() {
    let (owned, marker, home) = setup();
    let folder = confirmed_memory_folder(&home, &marker);
    let checkout = folder.parent().unwrap();
    git(
        checkout,
        &[
            "commit",
            "--allow-empty",
            "-m",
            "Advance the unimported tip",
        ],
    );
    work(&home, &marker, &["add", "task to publish"]);
    let rebuild = ["--include-memories", "--rebuild", "--json"];
    success(sync_output(&home, &marker, "export", &folder, &rebuild));
    git(checkout, &["add", ".engram"]);
    git(checkout, &["commit", "-m", "Commit unbound rebuild"]);
    let head = combined_folder_head(&home);
    let before = files(&folder);
    let journal = combined_export_journal(&marker, checkout);
    let proof = fs::read(&journal).unwrap();

    for command in ["export", "import"] {
        let refused = sync_output(
            &home,
            &marker,
            command,
            &folder,
            &["--include-memories", "--json"],
        );
        assert!(!refused.status.success());
        let refusal = refusal_json(&refused);
        assert_eq!(
            refusal["error"]["code"],
            "task_sync_commit_context_required"
        );
        assert!(
            refusal["error"]["details"]["reason"]
                .as_str()
                .unwrap()
                .contains("import --basis OWN_EXPORT/project.json --include-memories")
        );
        assert_eq!(combined_folder_head(&home), head);
        assert_eq!(fs::read(&journal).unwrap(), proof);
        assert_eq!(files(&folder), before);
    }

    let basis = owned.path().join("saved-project.json");
    fs::copy(folder.join("project.json"), &basis).unwrap();
    let retry = [
        "--basis",
        basis.to_str().unwrap(),
        "--include-memories",
        "--json",
    ];
    success(sync_output(&home, &marker, "import", &folder, &retry));
    assert_ne!(combined_folder_head(&home), head);
    assert_eq!(files(&folder), before);
}

#[test]
fn memory_sync_cli_invalid_proof_rebuild_names_combined_saved_basis() {
    let (owned, marker, home) = setup();
    let folder = confirmed_memory_folder(&home, &marker);
    let checkout = folder.parent().unwrap();
    work(&home, &marker, &["add", "later task"]);
    let rebuild = ["--include-memories", "--rebuild", "--json"];
    success(sync_output(&home, &marker, "export", &folder, &rebuild));
    git(checkout, &["add", ".engram"]);
    git(checkout, &["commit", "-m", "Commit the combined rebuild"]);
    let journal = combined_export_journal(&marker, checkout);
    fs::write(&journal, b"{").unwrap();
    let head = combined_folder_head(&home);
    let before = files(&folder);
    let refused = sync_output(&home, &marker, "export", &folder, &rebuild);
    assert!(!refused.status.success());
    let refusal = refusal_json(&refused);
    assert_eq!(
        refusal["error"]["code"],
        "task_sync_incremental_basis_unavailable"
    );
    assert!(
        refusal["error"]["details"]["reason"]
            .as_str()
            .unwrap()
            .contains("import --basis OWN_EXPORT/project.json --include-memories")
    );
    assert_eq!(combined_folder_head(&home), head);
    assert_eq!(fs::read(&journal).unwrap(), b"{");
    assert_eq!(files(&folder), before);

    let basis = owned.path().join("saved-project.json");
    fs::copy(folder.join("project.json"), &basis).unwrap();
    let retry = [
        "--basis",
        basis.to_str().unwrap(),
        "--include-memories",
        "--json",
    ];
    success(sync_output(&home, &marker, "import", &folder, &retry));
    assert_ne!(combined_folder_head(&home), head);
    assert!(!journal.exists());
    assert_eq!(files(&folder), before);
}

fn combined_folder_head(home: &Path) -> engram::ObjectId {
    let project = ProjectId("memory-cli-fixture".into());
    let profile = ExportProfile {
        include_notes: false,
        origin_machine: None,
    };
    SqliteStore::open(project_database_path(home, &project))
        .unwrap()
        .task_sync_folder_head(&project, &profile)
        .unwrap()
        .unwrap()
        .0
}

fn combined_data_worktree(marker: &Path, checkout: &Path) -> DataWorktree {
    DataWorktree::observe(marker.parent().unwrap(), checkout, "engram-data").unwrap()
}

pub(super) fn combined_export_journal(marker: &Path, checkout: &Path) -> std::path::PathBuf {
    let data = combined_data_worktree(marker, checkout);
    fs::read_dir(data.git_directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(OWNED_JOURNAL_PREFIX))
        })
        .expect("combined export retains its commit proof")
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
        refusal_json(&refused)["error"]["code"],
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
        refusal_json(&refused)["error"]["code"],
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
