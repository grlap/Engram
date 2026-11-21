use super::*;
use std::{
    path::PathBuf,
    process::{Child, Output},
    time::{Duration, Instant},
};

fn run_sync(host: &Host, options: &[&str]) -> Output {
    command(&host.home, &host.marker)
        .args([
            "task-sync",
            "--actor-id",
            "sync-author",
            "--session-id",
            "sync-session",
            "sync",
            "--json",
        ])
        .args(options)
        .output()
        .unwrap()
}

fn command_sync(host: &Host, options: &[&str]) -> Value {
    let result = run_sync(host, options);
    assert!(
        result.status.success(),
        "sync {options:?}: {} {}",
        String::from_utf8_lossy(&result.stderr),
        String::from_utf8_lossy(&result.stdout)
    );
    serde_json::from_slice(&result.stdout).unwrap()
}

fn enroll(root: &Path, with_memories: bool) -> (Host, Host, std::path::PathBuf) {
    let a = Host::new(root, "a");
    let b = Host::new(root, "b");
    let remote = root.join("data.git");
    fs::create_dir(&remote).unwrap();
    data_worktree::git(&remote, &["init", "--bare"]);
    let remote_name = remote.to_str().unwrap();
    a.git(&["remote", "add", "origin", remote_name]);
    b.git(&["remote", "add", "origin", remote_name]);
    let profile: &[&str] = if with_memories {
        &["--include-memories"]
    } else {
        &[]
    };
    a.work(&["add", "Shared setup task"]);
    a.receipt("export", profile);
    let basis = root.join("a-basis.json");
    fs::copy(a.folder().join("project.json"), &basis).unwrap();
    a.commit("Publish the fixture's initial planning");
    let mut local_options = profile.to_vec();
    local_options.extend(["--basis", basis.to_str().unwrap()]);
    a.receipt("import", &local_options);
    b.merge_from(&a);
    let mut join_options = profile.to_vec();
    join_options.push("--empty-basis");
    b.receipt("import", &join_options);
    a.git(&[
        "-c",
        "protocol.file.allow=always",
        "push",
        "origin",
        "HEAD:refs/heads/engram-data",
    ]);
    (a, b, remote)
}

#[test]
fn one_command_previews_imports_before_push_and_recovers_from_no_push() {
    let fixture = test_support::temp_home().unwrap();
    let (a, b, remote) = enroll(fixture.path(), false);
    let original = a.head();
    a.work(&["add", "Native addition"]);
    let preview = command_sync(&a, &["--dry-run"]);
    assert_eq!(preview["local_head"], original);
    assert_eq!(preview["remote_head"], original);
    assert_eq!(a.head(), original);

    let saved = command_sync(&a, &["--no-push"]);
    assert!(saved["imported"].is_object());
    assert_eq!(saved["pushed"], false);
    let selected = a.head();
    assert_ne!(selected, original);
    let remote_tip = data_worktree::git(&remote, &["rev-parse", "refs/heads/engram-data"]);
    assert_eq!(String::from_utf8(remote_tip).unwrap().trim(), original);

    let published = command_sync(&a, &[]);
    assert_eq!(published["pushed"], true);
    assert_eq!(published["local_head"], selected);
    assert_eq!(a.head(), selected);
    let joined = command_sync(&b, &[]);
    assert!(joined["imported"].is_object());
    assert_eq!(b.head(), selected);
}

#[test]
fn one_command_refuses_legacy_only_merge_with_explicit_rebuild_guidance() {
    let fixture = test_support::temp_home().unwrap();
    let (a, b, remote) = enroll(fixture.path(), false);
    b.work(&["add", "Planning selected by the peer"]);
    let peer = command_sync(&b, &[])["local_head"]
        .as_str()
        .unwrap()
        .to_owned();

    let project_path = a.folder().join("project.json");
    let mut project: Value = serde_json::from_slice(&fs::read(&project_path).unwrap()).unwrap();
    let exchange = project["folder_exchange"].as_object_mut().unwrap();
    assert!(exchange.remove("incremental").is_some());
    assert!(exchange.contains_key("export_basis"));
    fs::write(&project_path, serde_json::to_vec_pretty(&project).unwrap()).unwrap();
    a.commit("Keep the legacy folder basis");
    let local = a.head();

    let result = run_sync(&a, &[]);
    assert!(!result.status.success());
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(
        error.contains("task_sync_incremental_basis_unavailable"),
        "{error}"
    );
    assert!(
        error.contains("task-sync import --rebuild --basis"),
        "{error}"
    );
    assert_eq!(a.head(), local);
    let remote_head = data_worktree::git(&remote, &["rev-parse", "refs/heads/engram-data"]);
    assert_eq!(String::from_utf8(remote_head).unwrap().trim(), peer);
}

#[test]
fn one_command_refuses_unpublished_manual_history_and_policy_content() {
    let fixture = test_support::temp_home().unwrap();
    let (a, _, remote) = enroll(fixture.path(), false);
    let original = a.head();
    a.git(&[
        "-c",
        "author.name=Engram task sync",
        "-c",
        "author.email=task-sync@engram.invalid",
        "-c",
        "committer.name=Engram task sync",
        "-c",
        "committer.email=task-sync@engram.invalid",
        "commit",
        "--allow-empty",
        "-m",
        "Export project data changes",
    ]);
    let manual = a.head();
    let refused = run_sync(&a, &[]);
    assert!(!refused.status.success());
    let error = String::from_utf8_lossy(&refused.stderr);
    assert!(
        error.contains("will not publish commits it did not create"),
        "{error}"
    );
    assert_eq!(a.head(), manual);

    fs::write(
        a.folder().join(".gitignore"),
        "# This placeholder represents unpublished operator text.\n",
    )
    .unwrap();
    a.commit("Set an unpublished data policy");
    let unreviewed = a.head();
    let refused = run_sync(&a, &[]);
    assert!(!refused.status.success());
    let error = String::from_utf8_lossy(&refused.stderr);
    assert!(error.contains("policy disclosure refused"), "{error}");
    assert_eq!(a.head(), unreviewed);
    let observed = data_worktree::git(&remote, &["rev-parse", "refs/heads/engram-data"]);
    assert_eq!(String::from_utf8(observed).unwrap().trim(), original);
}

#[test]
fn one_command_refuses_to_publish_restricted_data_removed_from_the_tip() {
    let fixture = test_support::temp_home().unwrap();
    let (a, _, remote) = enroll(fixture.path(), false);
    let original = a.head();
    let reference = "refs/heads/engram-data";
    restricted_history(&a);
    let tip = a.head();

    for expected_remote in [Some(original.as_str()), None] {
        if expected_remote.is_none() {
            data_worktree::git(&remote, &["update-ref", "-d", reference]);
        }
        let output = run_sync(&a, &[]);
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("task disclosure refused"), "{error}");
        assert_eq!(a.head(), tip);
        let observed = data_worktree::git(
            &remote,
            &["for-each-ref", "--format=%(objectname)", reference],
        );
        assert_eq!(
            String::from_utf8(observed).unwrap().trim(),
            expected_remote.unwrap_or_default()
        );
    }
}

fn restricted_history(a: &Host) {
    let task_file = fs::read_dir(a.folder().join("tasks"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let public = fs::read(&task_file).unwrap();
    let mut restricted: Value = serde_json::from_slice(&public).unwrap();
    restricted["labels"] = serde_json::json!(["restricted"]);
    fs::write(&task_file, serde_json::to_vec(&restricted).unwrap()).unwrap();
    a.commit("Create restricted historical planning");
    fs::write(task_file, public).unwrap();
    a.commit("Restore the published task document");
}

#[test]
fn one_command_refuses_a_historical_memory_without_its_local_target() {
    let fixture = test_support::temp_home().unwrap();
    let (a, _, remote) = enroll(fixture.path(), true);
    let original = a.head();
    let task = fs::read_dir(a.folder().join("tasks"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let target = format!("local:{}", task.file_stem().unwrap().to_str().unwrap());
    a.work(&[
        "remember",
        "Planning tied to the task",
        "--key",
        "local-target",
        "--retires-with",
        &target,
    ]);
    command_sync(&a, &["--no-push"]);
    let selected = fs::read(&task).unwrap();
    fs::remove_file(&task).unwrap();
    a.commit("Leave the historical memory without its task");
    fs::write(task, selected).unwrap();
    a.commit("Restore the task in the visible tip");
    let local = a.head();

    let output = run_sync(&a, &[]);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("memory disclosure refused"), "{error}");
    assert_eq!(a.head(), local);
    let observed = data_worktree::git(&remote, &["rev-parse", "refs/heads/engram-data"]);
    assert_eq!(String::from_utf8(observed).unwrap().trim(), original);
}

#[test]
fn one_command_checks_staged_postimages_before_advancing_the_export_head() {
    let fixture = test_support::temp_home().unwrap();
    let (a, _, remote) = enroll(fixture.path(), false);
    let original = a.head();
    a.work(&["add", "Retain the literal $Id: fixture postimage $"]);
    a.receipt("export", &[]);
    let data = engram::task_sync::data_worktree::DataWorktree::observe_project(
        &a.marker,
        &ProjectId(PROJECT.into()),
        "engram-data",
    )
    .unwrap();
    let info = data.common_directory.join("info");
    fs::create_dir_all(&info).unwrap();
    // The built-in ident conversion is independent of external filter drivers.
    fs::write(info.join("attributes"), ".engram/tasks/*.json ident\n").unwrap();

    let result = run_sync(&a, &[]);
    assert!(!result.status.success());
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(error.contains("staged files do not match"), "{error}");
    assert_eq!(a.head(), original);
    let remote_tip = data_worktree::git(&remote, &["rev-parse", "refs/heads/engram-data"]);
    assert_eq!(String::from_utf8(remote_tip).unwrap().trim(), original);
}

#[test]
fn one_command_preserves_incompatible_staged_export_edits() {
    let fixture = test_support::temp_home().unwrap();
    let (a, _, _) = enroll(fixture.path(), false);
    let original = a.head();
    let title = "Retained task to stage";
    a.work(&["add", title]);
    a.receipt("export", &[]);
    let task = fs::read_dir(a.folder().join("tasks"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            serde_json::from_slice::<Value>(&fs::read(path).unwrap()).unwrap()["title"] == title
        })
        .unwrap();
    let path = format!(
        ".engram/tasks/{}",
        task.file_name().unwrap().to_str().unwrap()
    );
    let retained = fs::read(&task).unwrap();
    let mut staged: Value = serde_json::from_slice(&retained).unwrap();
    staged["title"] = "Operator's staged-only change".into();
    fs::write(&task, serde_json::to_vec(&staged).unwrap()).unwrap();
    a.git(&["add", "--", &path]);
    fs::write(&task, &retained).unwrap();
    let staged_path = format!(":{path}");
    let before = data_worktree::git(&a.data(), &["show", &staged_path]);

    let result = run_sync(&a, &[]);
    assert!(!result.status.success());
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(error.contains("existing index differs"), "{error}");
    assert_eq!(a.head(), original);
    assert_eq!(fs::read(task).unwrap(), retained);
    assert_eq!(
        data_worktree::git(&a.data(), &["show", &staged_path]),
        before
    );
}

#[test]
fn one_command_keeps_a_merge_pending_when_its_memory_resolution_is_invalid() {
    let fixture = test_support::temp_home().unwrap();
    let (a, b, remote) = enroll(fixture.path(), true);
    let task = fs::read_dir(a.folder().join("tasks"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let id = task.file_stem().unwrap().to_str().unwrap();
    a.work(&["update", id, "--title", "Shared setup task edited on A"]);
    a.work(&["remember", "Local memory", "--key", "merge-memory"]);
    b.work(&["update", id, "--title", "Shared setup task edited on B"]);
    command_sync(&a, &["--no-push"]);
    let local = a.head();
    let selected_task = fs::read(&task).unwrap();
    assert_eq!(command_sync(&b, &[])["pushed"], true);
    let peer = b.head();
    assert!(!run_sync(&a, &[]).status.success());
    let memory = fs::read_dir(a.folder().join("memories"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join("head.json");
    assert!(memory.is_file());
    fs::write(task, selected_task).unwrap();
    fs::write(&memory, b"malformed memory resolution\n").unwrap();

    assert!(!run_sync(&a, &["--reconciled"]).status.success());
    assert_eq!(a.head(), local);
    let pending = data_worktree::git(&a.data(), &["rev-parse", "--verify", "MERGE_HEAD"]);
    assert_eq!(String::from_utf8(pending).unwrap().trim(), peer);
    assert_eq!(fs::read(memory).unwrap(), b"malformed memory resolution\n");
    let observed = data_worktree::git(&remote, &["rev-parse", "refs/heads/engram-data"]);
    assert_eq!(String::from_utf8(observed).unwrap().trim(), peer);
}

#[test]
fn one_command_refuses_fetched_policy_changes_before_checkout() {
    let fixture = test_support::temp_home().unwrap();
    let (a, b, _) = enroll(fixture.path(), false);
    let local = a.head();
    let policy = a.folder().join(".gitattributes");
    let original_policy = fs::read(&policy).ok();
    fs::write(
        b.folder().join(".gitattributes"),
        "*.json filter=fixture-driver\n",
    )
    .unwrap();
    b.commit("Change the peer attribute policy");
    b.git(&[
        "-c",
        "protocol.file.allow=always",
        "push",
        "origin",
        "HEAD:refs/heads/engram-data",
    ]);

    let output = run_sync(&a, &[]);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("changes shared Git policy"), "{error}");
    assert_eq!(a.head(), local);
    assert_eq!(fs::read(policy).ok(), original_policy);
}

#[cfg(unix)]
#[test]
fn one_command_refuses_a_linked_project_file_when_resuming_a_merge() {
    let fixture = test_support::temp_home().unwrap();
    let (a, b, remote) = enroll(fixture.path(), false);
    let entries = fs::read_dir(a.folder().join("tasks")).unwrap();
    let task = entries.into_iter().next().unwrap().unwrap().path();
    let id = task.file_stem().unwrap().to_str().unwrap();
    a.work(&["update", id, "--title", "Shared setup task edited on A"]);
    b.work(&["update", id, "--title", "Shared setup task edited on B"]);
    command_sync(&a, &["--no-push"]);
    let local = a.head();
    assert_eq!(command_sync(&b, &[])["pushed"], true);
    let peer = b.head();
    assert!(!run_sync(&a, &[]).status.success());
    assert!(
        !data_worktree::git(&a.data(), &["ls-files", "-u", "--", ".engram/project.json"])
            .is_empty()
    );

    let envelope = a.folder().join("project.json");
    let outside = fixture.path().join("selected-outside-file");
    let sentinel = b"outside file stays intact\n";
    fs::write(&outside, sentinel).unwrap();
    fs::remove_file(&envelope).unwrap();
    std::os::unix::fs::symlink(&outside, &envelope).unwrap();

    assert!(!run_sync(&a, &["--reconciled"]).status.success());
    assert!(fs::symlink_metadata(envelope).unwrap().is_symlink());
    assert_eq!(fs::read(outside).unwrap(), sentinel);
    assert_eq!(a.head(), local);
    let remote_tip = data_worktree::git(&remote, &["rev-parse", "refs/heads/engram-data"]);
    assert_eq!(String::from_utf8(remote_tip).unwrap().trim(), peer);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn one_command_preserves_the_pending_merge_after_a_runner_failure() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = test_support::temp_home().unwrap();
    let (a, b, remote) = enroll(fixture.path(), false);
    a.work(&["add", "Local planning"]);
    b.work(&["add", "Peer planning"]);
    command_sync(&a, &["--no-push"]);
    let local = a.head();
    assert_eq!(command_sync(&b, &[])["pushed"], true);
    let peer = b.head();
    let path = std::env::var_os("PATH").unwrap();
    let git = std::env::split_paths(&path)
        .map(|dir| dir.join("git"))
        .find(|file| file.is_file())
        .unwrap();
    let bin = fixture.path().join("controlled-bin");
    fs::create_dir(&bin).unwrap();
    let wrapper = bin.join("git");
    // An actual Git merge selects the pending peer. The wrapper then reports
    // a fatal status, which cannot authorize further writes to that merge.
    fs::write(
        &wrapper,
        br#"#!/bin/sh
for arg do
    if [ "$arg" = merge ]; then
        "$ENGRAM_FIXTURE_REAL_GIT" "$@"
        exit 128
    fi
done
exec "$ENGRAM_FIXTURE_REAL_GIT" "$@"
"#,
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    let path =
        std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&path))).unwrap();
    let output = command(&a.home, &a.marker)
        .args([
            "task-sync",
            "--actor-id",
            "sync-author",
            "--session-id",
            "sync-session",
            "sync",
            "--json",
        ])
        .env("PATH", path)
        .env("ENGRAM_FIXTURE_REAL_GIT", git)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(a.head(), local);
    let merge_head = data_worktree::git(&a.data(), &["rev-parse", "--verify", "MERGE_HEAD"]);
    assert_eq!(String::from_utf8(merge_head).unwrap().trim(), peer);
    let pending = data_worktree::git(&a.data(), &["ls-files", "--unmerged", "--", ".engram"]);
    assert!(!pending.is_empty());
    let remote_tip = data_worktree::git(&remote, &["rev-parse", "refs/heads/engram-data"]);
    assert_eq!(String::from_utf8(remote_tip).unwrap().trim(), peer);
}

#[test]
fn one_command_merges_disjoint_task_and_memory_updates_before_pushing() {
    let fixture = test_support::temp_home().unwrap();
    let (a, b, _) = enroll(fixture.path(), true);
    a.work(&["add", "Planning from A"]);
    a.work(&[
        "remember",
        "--key",
        "a-shared-context",
        "A owns the first check.",
    ]);
    b.work(&["add", "Planning from B"]);
    b.work(&[
        "remember",
        "--key",
        "b-shared-context",
        "B owns the second check.",
    ]);
    assert_eq!(
        command_sync(&a, &["--no-push"])["scope"],
        "tasks_and_memories"
    );
    assert_eq!(command_sync(&b, &[])["pushed"], true);
    let merged = command_sync(&a, &[]);
    assert_eq!(merged["pushed"], true);
    assert!(merged["imported"].is_object());
    assert_eq!(command_sync(&b, &[])["local_head"], a.head());
    let folder = directory::read_resolved_data_folder_with_memories(
        &engram::task_sync::data_worktree::DataWorktree::observe_project(
            &a.marker,
            &ProjectId(PROJECT.into()),
            "engram-data",
        )
        .unwrap(),
    )
    .unwrap();
    let titles = folder
        .tree
        .tasks
        .iter()
        .map(|task| task.title.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        titles,
        ["Shared setup task", "Planning from A", "Planning from B"].into()
    );
    for (host, key) in [(&a, "b-shared-context"), (&b, "a-shared-context")] {
        let selected = command(&host.home, &host.marker)
            .args(["work", "memories", key, "--full"])
            .output()
            .unwrap();
        assert!(
            selected.status.success(),
            "{}",
            String::from_utf8_lossy(&selected.stderr)
        );
    }
}

#[test]
fn one_command_refuses_configured_custom_merge_drivers_before_merging() {
    let fixture = test_support::temp_home().unwrap();
    let (a, b, remote) = enroll(fixture.path(), false);
    a.work(&["add", "Planning that remains on A"]);
    b.work(&["add", "Planning published by B"]);
    command_sync(&a, &["--no-push"]);
    command_sync(&b, &[]);
    let local = a.head();
    let peer = b.head();
    a.git(&["config", "merge.default", "ordinary-fixture"]);
    a.git(&["config", "merge.ordinary-fixture.driver", "exit 97"]);

    let output = run_sync(&a, &[]);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("configured custom Git merge drivers"),
        "{error}"
    );
    assert_eq!(a.head(), local);
    let data = engram::task_sync::data_worktree::DataWorktree::observe_project(
        &a.marker,
        &ProjectId(PROJECT.into()),
        "engram-data",
    )
    .unwrap();
    assert!(!data.git_directory.join("MERGE_HEAD").exists());
    let observed = data_worktree::git(&remote, &["rev-parse", "refs/heads/engram-data"]);
    assert_eq!(String::from_utf8(observed).unwrap().trim(), peer);
}

#[test]
fn one_command_imports_the_committed_memory_under_the_recorded_source() {
    let fixture = test_support::temp_home().unwrap();
    let (a, b, _) = enroll(fixture.path(), true);
    let key = "selected-memory";
    let versions = a.folder().join("memories").join(key);
    a.work(&["remember", "--key", key, "Older loose memory"]);
    command_sync(&a, &[]);
    let old_head = fs::read(versions.join("head.json")).unwrap();
    let old_files = fs::read_dir(&versions)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<std::collections::BTreeSet<_>>();

    a.work(&[
        "remember",
        "--key",
        key,
        "--revise",
        "Current committed memory",
    ]);
    command_sync(&a, &[]);
    let selected = a.head();
    b.merge_from(&a);
    let b_versions = b.folder().join("memories").join(key);
    let new_files = fs::read_dir(&b_versions)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| !old_files.contains(name))
        .collect::<Vec<_>>();
    assert_eq!(new_files.len(), 1);
    let hidden = new_files[0].to_str().unwrap();
    b.git(&[
        "update-index",
        "--assume-unchanged",
        "--",
        &format!(".engram/memories/{key}/head.json"),
        &format!(".engram/memories/{key}/{hidden}"),
    ]);
    fs::write(b_versions.join("head.json"), old_head).unwrap();
    fs::remove_file(b_versions.join(hidden)).unwrap();
    assert!(data_worktree::git(&b.data(), &["status", "--porcelain"]).is_empty());

    let receipt = command_sync(&b, &["--no-push"]);
    assert_eq!(receipt["local_head"], selected);
    assert_eq!(receipt["imported"]["route"], "incremental");
    let project = ProjectId(PROJECT.into());
    let profile = engram::task_sync::progress::ExportProfile {
        include_notes: false,
        origin_machine: None,
    };
    let store = SqliteStore::open(project_database_path(&b.home, &project)).unwrap();
    let (_, recorded) = store
        .task_sync_folder_head(&project, &profile)
        .unwrap()
        .unwrap();
    assert_eq!(recorded.source.unwrap().commit, selected);
    let memory = work(&b.home, &b.marker, &["memories", key, "--full", "--json"]);
    let memory: Value = serde_json::from_slice(&memory.stdout).unwrap();
    assert_eq!(memory["body"], "Current committed memory");
}

#[test]
fn one_command_refetches_after_a_rejected_push() {
    let fixture = test_support::temp_home().unwrap();
    let root = fixture.path();
    let (a, b, remote) = enroll(root, false);
    b.work(&["add", "Peer planning before the race"]);
    command_sync(&b, &["--no-push"]);
    let peer = b.head();
    let (_server, url) = loopback_server(root, &remote);
    select_loopback(&a, root, &url);
    fs::write(
        root.join("race.json"),
        serde_json::to_vec(&serde_json::json!({ "repository": b.data(), "commit": peer })).unwrap(),
    )
    .unwrap();
    a.work(&["add", "Local planning before the race"]);
    let receipt = command_sync(&a, &[]);
    assert!(root.join("race-fired.json").is_file());
    assert_eq!(receipt["pushed"], true);
    assert!(receipt["imported"].is_object());
    let snapshot =
        engram::task_sync::git::read_task_snapshot(&remote, &ProjectId(PROJECT.into()), &a.head())
            .unwrap();
    assert!(snapshot.commit.parents.contains(&peer));
    let titles = snapshot
        .tree
        .tasks
        .iter()
        .map(|task| task.title.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        titles,
        [
            "Shared setup task",
            "Peer planning before the race",
            "Local planning before the race"
        ]
        .into()
    );
}

#[test]
fn one_command_leaves_publication_pending_if_the_audited_remote_ref_is_deleted() {
    let fixture = test_support::temp_home().unwrap();
    let root = fixture.path();
    let (a, _, remote) = enroll(root, false);
    restricted_history(&a);
    let observed = a.head();
    let reference = "refs/heads/engram-data";
    a.git(&[
        "-c",
        "protocol.file.allow=always",
        "push",
        "origin",
        &format!("{observed}:{reference}"),
    ]);
    command_sync(&a, &["--no-push"]);
    a.work(&["add", "A new ordinary sync commit"]);
    command_sync(&a, &["--no-push"]);
    let local = a.head();
    assert_ne!(local, observed);
    let (_server, url) = loopback_server(root, &remote);
    select_loopback(&a, root, &url);
    fs::write(
        root.join("race.json"),
        serde_json::to_vec(&serde_json::json!({ "delete": observed })).unwrap(),
    )
    .unwrap();

    let output = run_sync(&a, &[]);
    assert!(
        !output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["pushed"], false);
    assert!(
        receipt["pending"]
            .as_str()
            .unwrap()
            .contains("audited data tip")
    );
    assert!(root.join("race-fired.json").is_file());
    assert_eq!(a.head(), local);
    assert!(
        data_worktree::git(
            &remote,
            &["for-each-ref", "--format=%(objectname)", reference],
        )
        .is_empty()
    );
    assert!(
        !fs::read_to_string(root.join("requests"))
            .unwrap()
            .contains("POST /data.git/git-receive-pack")
    );
}

fn select_loopback(host: &Host, root: &Path, url: &str) {
    host.git(&["remote", "set-url", "origin", url]);
    host.git(&["config", "--local", "credential.helper", ""]);
    host.git(&[
        "config",
        "--local",
        "http.sslCAInfo",
        root.join("cert.pem").to_str().unwrap(),
    ]);
    #[cfg(windows)]
    host.git(&["config", "--local", "http.sslBackend", "openssl"]);
}

#[test]
fn one_command_checks_retained_export_bytes_before_committing_and_merging() {
    let fixture = test_support::temp_home().unwrap();
    let (a, b, remote) = enroll(fixture.path(), false);
    let original = a.head();
    a.work(&["add", "Retained planning from A"]);
    a.receipt("export", &[]);
    let envelope = a.folder().join("project.json");
    let exported = fs::read(&envelope).unwrap();
    let edited = [&exported[..], b"\n "].concat();
    fs::write(&envelope, &edited).unwrap();

    let refused = run_sync(&a, &[]);
    assert!(!refused.status.success());
    assert_eq!(a.head(), original);
    assert_eq!(fs::read(&envelope).unwrap(), edited);

    fs::write(&envelope, exported).unwrap();
    b.work(&["add", "Peer planning from B"]);
    assert_eq!(command_sync(&b, &[])["pushed"], true);
    let peer = b.head();
    let receipt = command_sync(&a, &[]);
    assert_eq!(receipt["exported"], true);
    assert_eq!(receipt["pushed"], true);
    let snapshot =
        engram::task_sync::git::read_task_snapshot(&remote, &ProjectId(PROJECT.into()), &a.head())
            .unwrap();
    assert!(snapshot.commit.parents.contains(&peer));
    let titles = snapshot
        .tree
        .tasks
        .iter()
        .map(|task| task.title.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        titles,
        [
            "Shared setup task",
            "Retained planning from A",
            "Peer planning from B"
        ]
        .into()
    );
}

struct LoopbackServer(Child);

impl Drop for LoopbackServer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn loopback_server(root: &Path, remote: &Path) -> (LoopbackServer, String) {
    data_worktree::git(remote, &["config", "http.receivepack", "true"]);
    let exec = String::from_utf8(data_worktree::git(root, &["--exec-path"])).unwrap();
    let exec = PathBuf::from(exec.trim());
    let openssl = exec
        .ancestors()
        .map(|parent| parent.join("usr/bin/openssl.exe"))
        .find(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("openssl"));
    let cert = Command::new(openssl)
        .current_dir(root)
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "2",
            "-subj",
            "/CN=Engram loopback fixture",
            "-addext",
            "subjectAltName=IP:127.0.0.1",
            "-keyout",
            "key.pem",
            "-out",
            "cert.pem",
        ])
        .output()
        .unwrap();
    assert!(
        cert.status.success(),
        "{}",
        String::from_utf8_lossy(&cert.stderr)
    );
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/task_sync/exchange/tests/windows/data_branch/server.mjs");
    let mut server = LoopbackServer(Command::new("node").arg(script).arg(root).spawn().unwrap());
    let started = Instant::now();
    loop {
        if let Ok(record) = fs::read_to_string(root.join("endpoint"))
            && let Some(url) = record.strip_suffix('\n')
        {
            return (server, url.to_owned());
        }
        assert!(
            server.0.try_wait().unwrap().is_none(),
            "the loopback server exited before it reported an endpoint"
        );
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the loopback server did not report an endpoint"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
