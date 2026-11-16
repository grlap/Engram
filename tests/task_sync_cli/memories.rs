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
            let error: Value = serde_json::from_slice(&refused.stderr).unwrap();
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
    // Export the new task, capturing the exact capability envelope this CLI
    // writes. Then model a capability publication interrupted while staging
    // that envelope: the current envelope without the capability, and half
    // of the staged bytes. A routine task-only export is deliberately
    // forbidden on this memory-capable HEAD.
    success(sync_output(
        &home,
        &marker,
        "export",
        &folder,
        &["--include-memories", "--json"],
    ));
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
    // The store changes before the retry, so the retry writes a newer
    // envelope than the one staged.
    work(&home, &marker, &["add", "third task"]);
    let args = [
        "--include-memories",
        "--memory-recovery-basis",
        retained.to_str().unwrap(),
        "--json",
    ];
    success(sync_output(&home, &marker, "export", &folder, &args));
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
