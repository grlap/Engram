use super::*;

fn run_sync(host: &Host, extra: &[&str]) -> Value {
    let output = ordinary_sync_output(host, extra);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "sync {extra:?}: {stderr}");
    assert!(stderr.contains("development no-op"), "{stderr}");
    serde_json::from_slice(&output.stdout).unwrap()
}

fn ordinary_sync_output(host: &Host, extra: &[&str]) -> Output {
    command(&host.home, &host.marker)
        .args([
            "task-sync",
            "--actor-id",
            "operator",
            "--session-id",
            "sync-operator",
            "sync",
            "--json",
        ])
        .args(extra)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            host.checkout().join(".gitconfig-absent"),
        )
        .output()
        .unwrap()
}

fn enrolled_hosts(root: &Path, export_flags: &[&str]) -> (Host, Host, std::path::PathBuf) {
    let local = Host::new(root, "local");
    let peer = Host::new(root, "peer");
    let remote = root.join("data.git");
    fs::create_dir(&remote).unwrap();
    data_worktree::git(&remote, &["init", "--bare"]);
    for host in [&local, &peer] {
        host.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
    }

    local.work(&["add", "Shared task"]);
    assert_eq!(local.receipt("export", export_flags)["route"], "bootstrap");
    local.commit("Seed shared tasks");
    local.receipt(
        "import",
        &[
            "--rebuild",
            "--basis",
            local.folder().join("project.json").to_str().unwrap(),
        ],
    );
    let seed = local.head();
    let published = run_sync(&local, &[]);
    assert_eq!(published["pushed"], true);
    let remote_tip = data_worktree::git(&remote, &["rev-parse", "refs/heads/engram-data"]);
    assert_eq!(String::from_utf8(remote_tip).unwrap().trim(), seed);
    assert_eq!(local.head(), seed);
    peer.merge_from(&local);
    peer.receipt("import", &["--empty-basis"]);
    (local, peer, remote)
}

fn memory_body(host: &Host, key: &str) -> String {
    let output = work(
        &host.home,
        &host.marker,
        &["memories", key, "--full", "--json"],
    );
    let memory: Value = serde_json::from_slice(&output.stdout).unwrap();
    memory["body"].as_str().unwrap().to_owned()
}

fn remote_tip(remote: &Path) -> String {
    let output = data_worktree::git(remote, &["rev-parse", "refs/heads/engram-data"]);
    String::from_utf8(output).unwrap().trim().to_owned()
}

#[test]
fn one_command_opts_in_then_exchanges_memories_without_a_flag() {
    let fixture = test_support::temp_home().unwrap();
    let (local, peer, remote) = enrolled_hosts(fixture.path(), &[]);
    let seed = peer.head();
    peer.work(&["add", "Peer task before memory opt-in"]);
    assert_eq!(run_sync(&peer, &["--no-push"])["pushed"], false);
    let task_only_tip = peer.head();
    local.work(&["remember", "First rule", "--key", "rule"]);
    assert_eq!(run_sync(&local, &["--include-memories"])["pushed"], true);
    let enabled_tip = local.head();
    assert_ne!(enabled_tip, seed);
    assert_eq!(run_sync(&peer, &[])["pushed"], true);
    let parents = data_worktree::git(&peer.data(), &["show", "-s", "--format=%P", "HEAD"]);
    let parents = String::from_utf8(parents).unwrap();
    assert_eq!(
        parents.split_whitespace().collect::<Vec<_>>(),
        [task_only_tip.as_str(), enabled_tip.as_str()]
    );
    let envelope: Value =
        serde_json::from_slice(&fs::read(peer.folder().join("project.json")).unwrap()).unwrap();
    assert_eq!(
        envelope["memory_profile"],
        engram::task_sync::memories::FORMAT
    );
    assert_eq!(memory_body(&peer, "rule"), "First rule");
    assert_eq!(run_sync(&local, &[])["pushed"], false);
    assert_eq!(peer.head(), local.head());

    local.work(&["remember", "Revised rule", "--key", "rule", "--revise"]);
    let task = local.add_ref("Task with a memory edit");
    assert_eq!(run_sync(&local, &["--no-push"])["pushed"], false);
    let local_tip = local.head();
    assert_eq!(remote_tip(&remote), peer.head());
    assert_ne!(local_tip, peer.head());

    peer.work(&["remember", "Peer rule", "--key", "peer-rule"]);
    assert_eq!(run_sync(&peer, &[])["pushed"], true);
    let peer_tip = peer.head();

    assert_eq!(run_sync(&local, &[])["pushed"], true);
    let parents = data_worktree::git(&local.data(), &["show", "-s", "--format=%P", "HEAD"]);
    let parents = String::from_utf8(parents).unwrap();
    assert_eq!(
        parents.split_whitespace().collect::<Vec<_>>(),
        [local_tip.as_str(), peer_tip.as_str()]
    );
    assert_eq!(memory_body(&local, "peer-rule"), "Peer rule");
    assert_eq!(run_sync(&peer, &[])["pushed"], false);
    assert_eq!(memory_body(&peer, "rule"), "Revised rule");
    let shown = work(&peer.home, &peer.marker, &["show", &task, "--json"]);
    let shown: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(shown["status"]["work"]["title"], "Task with a memory edit");
    assert_eq!(peer.head(), local.head());
    assert_eq!(remote_tip(&remote), local.head());
}

fn conflict_with_memory_enablement(
    root: &Path,
) -> (Host, Host, std::path::PathBuf, engram::WorkId, String) {
    let (local, peer, remote) = enrolled_hosts(root, &[]);
    let task = directory::read_folder(&local.folder()).unwrap().tree.tasks[0].id;
    let task_ref = task.0.to_string();
    local.work(&["update", &task_ref, "--title", "Local change"]);
    run_sync(&local, &["--no-push"]);
    peer.work(&["update", &task_ref, "--title", "Peer change"]);
    peer.work(&["remember", "Keep the peer's rule", "--key", "peer-rule"]);
    run_sync(&peer, &["--include-memories"]);
    let enabled_tip = peer.head();
    assert!(peer.folder().join("memories/peer-rule/head.json").is_file());

    let output = ordinary_sync_output(&local, &["--no-push"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("the data merge has conflicts"), "{stderr}");
    (local, peer, remote, task, enabled_tip)
}

fn resolve_conflict_with_local_memories(local: &Host, task: engram::WorkId, envelope_from: &str) {
    local.git(&[
        "restore",
        "--source=HEAD",
        "--staged",
        "--worktree",
        "--",
        ".engram/memories",
        &format!(".engram/tasks/{}.json", task.0),
    ]);
    local.git(&[
        "restore",
        "--source",
        envelope_from,
        "--staged",
        "--worktree",
        "--",
        ".engram/project.json",
    ]);
    local.commit("Resolve the sync conflict with the local memory files");
}

#[test]
fn one_command_refuses_to_publish_a_merge_that_drops_the_memory_profile() {
    let fixture = test_support::temp_home().unwrap();
    let (local, _, remote, task, enabled_tip) = conflict_with_memory_enablement(fixture.path());
    resolve_conflict_with_local_memories(&local, task, "HEAD");
    let merged = local.head();
    let parents = data_worktree::git(&local.data(), &["show", "-s", "--format=%P", "HEAD"]);
    let parents = String::from_utf8(parents).unwrap();
    assert_eq!(
        parents.split_whitespace().nth(1),
        Some(enabled_tip.as_str())
    );

    assert!(!local.folder().join("memories/peer-rule/head.json").exists());

    for options in [
        &["--no-push"][..],
        &[][..],
        &["--include-memories", "--no-push"][..],
        &["--include-memories"][..],
    ] {
        let output = ordinary_sync_output(&local, options);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{stderr}");
        assert!(stderr.contains("drops its memory profile"), "{stderr}");
        assert_eq!(local.head(), merged);
        assert_eq!(remote_tip(&remote), enabled_tip);
    }
}

#[test]
fn one_command_refuses_to_publish_a_capable_merge_that_drops_a_peer_key() {
    let fixture = test_support::temp_home().unwrap();
    let (local, _, remote, task, enabled_tip) = conflict_with_memory_enablement(fixture.path());
    resolve_conflict_with_local_memories(&local, task, &enabled_tip);
    let merged = local.head();
    let basis = directory::read_folder_basis(&local.folder().join("project.json")).unwrap();
    assert!(basis.memory_capable);
    assert!(!local.folder().join("memories/peer-rule/head.json").exists());

    for options in [&["--no-push"][..], &[][..]] {
        let output = ordinary_sync_output(&local, options);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{stderr}");
        assert!(stderr.contains("drops memory keys"), "{stderr}");
        assert!(
            stderr.contains(".engram/memories/peer-rule/head.json"),
            "{stderr}"
        );
        assert_eq!(local.head(), merged);
        assert_eq!(remote_tip(&remote), enabled_tip);
    }
}

#[test]
fn one_command_refuses_to_publish_a_resurrected_memory() {
    refuse_stale_memory_resolution(
        &["forget", "rule"],
        "changes the permanent retirement of memory `rule`",
    );
}

#[test]
fn one_command_refuses_to_publish_a_rolled_back_memory_revision() {
    refuse_stale_memory_resolution(
        &["remember", "Peer revision", "--key", "rule", "--revise"],
        "does not preserve the fetched revision history of memory `rule`",
    );
}

fn refuse_stale_memory_resolution(peer_edit: &[&str], refusal: &str) {
    let fixture = test_support::temp_home().unwrap();
    let (local, peer, remote, task) = enrolled_memory_hosts(fixture.path());
    let task_ref = task.0.to_string();
    local.work(&["update", &task_ref, "--title", "Local change"]);
    run_sync(&local, &["--no-push"]);
    let old_head = fs::read(local.folder().join("memories/rule/head.json")).unwrap();
    peer.work(&["update", &task_ref, "--title", "Peer change"]);
    peer.work(peer_edit);
    assert_eq!(run_sync(&peer, &[])["pushed"], true);
    let published = peer.head();
    assert_ne!(
        fs::read(peer.folder().join("memories/rule/head.json")).unwrap(),
        old_head
    );

    let output = ordinary_sync_output(&local, &["--no-push"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("the data merge has conflicts"), "{stderr}");
    resolve_conflict_with_local_memories(&local, task, "HEAD");
    let merged = local.head();
    let parent = data_worktree::git(&local.data(), &["rev-parse", "HEAD^2"]);
    assert_eq!(String::from_utf8(parent).unwrap().trim(), published);
    assert_eq!(
        fs::read(local.folder().join("memories/rule/head.json")).unwrap(),
        old_head
    );
    for options in [&["--no-push"][..], &[][..]] {
        let output = ordinary_sync_output(&local, options);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{stderr}");
        assert!(stderr.contains(refusal), "{stderr}");
        assert_eq!(local.head(), merged);
        assert_eq!(remote_tip(&remote), published);
        assert_eq!(memory_body(&local, "rule"), "Shared rule");
    }
}

fn enrolled_memory_hosts(root: &Path) -> (Host, Host, std::path::PathBuf, engram::WorkId) {
    let (local, peer, remote) = enrolled_hosts(root, &[]);
    let task = directory::read_folder(&local.folder()).unwrap().tree.tasks[0].id;
    local.work(&["remember", "Shared rule", "--key", "rule"]);
    assert_eq!(run_sync(&local, &["--include-memories"])["pushed"], true);
    run_sync(&peer, &[]);
    assert_eq!(memory_body(&peer, "rule"), "Shared rule");
    (local, peer, remote, task)
}

#[test]
fn one_command_refuses_to_publish_an_unseen_retirement() {
    let fixture = test_support::temp_home().unwrap();
    let (local, peer, remote, task) = enrolled_memory_hosts(fixture.path());
    let task_ref = task.0.to_string();
    local.work(&["update", &task_ref, "--title", "Local change"]);
    local.work(&["forget", "rule"]);
    run_sync(&local, &["--no-push"]);
    peer.work(&["update", &task_ref, "--title", "Peer change"]);
    peer.work(&["remember", "Peer revision", "--key", "rule", "--revise"]);
    assert_eq!(run_sync(&peer, &[])["pushed"], true);
    let published = peer.head();

    let conflict = ordinary_sync_output(&local, &["--no-push"]);
    let stderr = String::from_utf8_lossy(&conflict.stderr);
    assert!(!conflict.status.success(), "{stderr}");
    assert!(stderr.contains("the data merge has conflicts"), "{stderr}");
    resolve_conflict_with_local_memories(&local, task, "HEAD");
    let merged = local.head();
    for options in [&["--no-push"][..], &[][..]] {
        let output = ordinary_sync_output(&local, options);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{stderr}");
        assert!(
            stderr.contains("memory_retirement_import_required"),
            "{stderr}"
        );
        assert!(stderr.contains("--retire-unseen rule"), "{stderr}");
        assert_eq!(local.head(), merged);
        assert_eq!(remote_tip(&remote), published);
        assert_eq!(memory_body(&peer, "rule"), "Peer revision");
    }
}

#[test]
fn one_command_refuses_changed_immutable_memory_before_import() {
    let fixture = test_support::temp_home().unwrap();
    let (local, peer, remote, task, published) = conflict_with_memory_enablement(fixture.path());
    local.git(&[
        "restore",
        "--source=HEAD",
        "--staged",
        "--worktree",
        "--",
        &format!(".engram/tasks/{}.json", task.0),
    ]);
    local.git(&[
        "restore",
        "--source",
        &published,
        "--staged",
        "--worktree",
        "--",
        ".engram/project.json",
        ".engram/memories",
    ]);
    let memory_folder = local.folder().join("memories/peer-rule");
    let revision_path = fs::read_dir(memory_folder)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.file_name().is_some_and(|name| name != "head.json"))
        .unwrap();
    let mut revision: Value = serde_json::from_slice(&fs::read(&revision_path).unwrap()).unwrap();
    revision["provenance"]["source"]["reason"] = "Reattributed during merge".into();
    fs::write(
        &revision_path,
        format!("{}\n", serde_json::to_string_pretty(&revision).unwrap()),
    )
    .unwrap();
    local.commit("Resolve tasks and edit the peer revision attribution");
    assert_refuses_immutable_edit(&local, &remote, &published);

    peer.work(&["forget", "peer-rule"]);
    assert_eq!(run_sync(&peer, &[])["pushed"], true);
    let retired_tip = peer.head();
    assert_ne!(retired_tip, published);
    let peer_revision = peer
        .folder()
        .join("memories/peer-rule")
        .join(revision_path.file_name().unwrap());
    assert!(!peer_revision.exists());
    assert_refuses_immutable_edit(&local, &remote, &retired_tip);
}

#[track_caller]
fn assert_refuses_immutable_edit(local: &Host, remote: &Path, published: &str) {
    let merged = local.head();
    let project = ProjectId(PROJECT.into());
    for options in [&["--no-push"][..], &[][..]] {
        let output = ordinary_sync_output(local, options);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{stderr}");
        assert!(
            stderr.contains("changes the immutable memory revision"),
            "{stderr}"
        );
        assert_eq!(local.head(), merged);
        assert_eq!(remote_tip(remote), published);
        let store = SqliteStore::open(project_database_path(&local.home, &project)).unwrap();
        assert_eq!(
            store
                .project_memory_sync_head(&project, "peer-rule")
                .unwrap(),
            None
        );
    }
}

#[test]
fn one_command_keeps_the_peer_key_when_opting_in_after_a_peer() {
    let fixture = test_support::temp_home().unwrap();
    let (local, peer, remote) = enrolled_hosts(fixture.path(), &[]);
    peer.work(&["remember", "Peer rule", "--key", "peer-rule"]);
    run_sync(&peer, &["--include-memories"]);
    local.work(&["remember", "Local rule", "--key", "local-rule"]);

    assert_eq!(run_sync(&local, &["--include-memories"])["pushed"], true);
    assert_eq!(remote_tip(&remote), local.head());
    assert_eq!(memory_body(&local, "peer-rule"), "Peer rule");
    run_sync(&peer, &[]);
    assert_eq!(memory_body(&peer, "local-rule"), "Local rule");
    assert_eq!(memory_body(&peer, "peer-rule"), "Peer rule");
}

#[cfg(windows)]
#[test]
fn one_command_reports_a_retirement_when_export_journal_cleanup_fails() {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_SHARE_READ: u32 = 1;
    let fixture = test_support::temp_home().unwrap();
    let (local, peer, remote) = enrolled_hosts(fixture.path(), &[]);
    local.work(&["remember", "Temporary rule", "--key", "retiring-rule"]);
    run_sync(&local, &["--include-memories"]);
    run_sync(&peer, &[]);

    let project = ProjectId(PROJECT.into());
    let prior_revision = SqliteStore::open(project_database_path(&local.home, &project))
        .unwrap()
        .project_memory_sync_head(&project, "retiring-rule")
        .unwrap()
        .unwrap();
    local.work(&["add", "Pending local export"]);
    local.receipt("export", &["--include-memories"]);
    local.commit("Publish local task files");
    let proof = super::super::memories::combined_export_journal(&local.marker, &local.data());
    peer.work(&["forget", "retiring-rule"]);
    run_sync(&peer, &[]);
    let peer_tip = peer.head();
    local.merge_from(&peer);
    let merged = local.head();

    // Another reader allows journal reads but denies file deletion.
    let reader = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(&proof)
        .unwrap();
    let output = ordinary_sync_output(&local, &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("os error 32"), "{stderr}");
    let retirement =
        format!("Retired project memory `retiring-rule` with local revisions {prior_revision}");
    assert!(stderr.contains(&retirement), "{stderr}");
    assert!(proof.is_file());
    assert_eq!(local.head(), merged);
    assert_eq!(remote_tip(&remote), peer_tip);
    drop(reader);

    let output = ordinary_sync_output(&local, &["--no-push"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(!stderr.contains(&retirement), "{stderr}");
    assert_eq!(remote_tip(&remote), peer_tip);
}

#[test]
fn one_command_stops_on_conflicting_memory_revisions_before_push() {
    let fixture = test_support::temp_home().unwrap();
    let (local, peer, remote) = enrolled_hosts(fixture.path(), &[]);
    local.work(&["remember", "Common rule", "--key", "rule"]);
    local.work(&["remember", "Temporary rule", "--key", "retiring-rule"]);
    run_sync(&local, &["--include-memories"]);
    run_sync(&peer, &[]);

    let project = ProjectId(PROJECT.into());
    let prior_revision = SqliteStore::open(project_database_path(&local.home, &project))
        .unwrap()
        .project_memory_sync_head(&project, "retiring-rule")
        .unwrap()
        .unwrap();
    local.work(&["remember", "Local revision", "--key", "rule", "--revise"]);
    peer.work(&["remember", "Peer revision", "--key", "rule", "--revise"]);
    peer.work(&["forget", "retiring-rule"]);
    run_sync(&peer, &[]);
    let published = peer.head();

    let output = ordinary_sync_output(&local, &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("the data merge has conflicts"), "{stderr}");
    let unmerged = data_worktree::git(&local.data(), &["diff", "--name-only", "--diff-filter=U"]);
    assert!(
        String::from_utf8(unmerged)
            .unwrap()
            .lines()
            .any(|path| path == ".engram/memories/rule/head.json")
    );
    assert_eq!(remote_tip(&remote), published);

    // Accepting the peer's files in Git still leaves two distinct native
    // revisions. The memory import must also stop sync, including on retry.
    local.git(&[
        "restore",
        "--source",
        &published,
        "--staged",
        "--worktree",
        "--",
        ".engram/memories/rule",
    ]);
    local.git(&[
        "restore",
        "--source=HEAD",
        "--staged",
        "--worktree",
        "--",
        ".engram/project.json",
    ]);
    local.commit("Keep the peer's memory history in the merge");
    let merged = local.head();
    let retirement =
        format!("Retired project memory `retiring-rule` with local revisions {prior_revision}");
    for (options, first_import) in [(&["--no-push"][..], true), (&[][..], false)] {
        let output = ordinary_sync_output(&local, options);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{stderr}");
        assert!(stderr.contains("did not apply these memories"), "{stderr}");
        assert!(stderr.contains("memory histories diverge"), "{stderr}");
        assert!(
            stderr.contains("Rerun your original `engram task-sync sync`"),
            "{stderr}"
        );
        assert!(!stderr.contains("engram task-sync import"), "{stderr}");
        assert_eq!(stderr.contains(&retirement), first_import, "{stderr}");
        assert_eq!(local.head(), merged);
        assert_eq!(remote_tip(&remote), published);
        assert_eq!(memory_body(&local, "rule"), "Local revision");
        assert_eq!(memory_body(&peer, "rule"), "Peer revision");
    }
}

#[test]
fn one_command_does_not_import_an_ignored_memory_tombstone() {
    let fixture = test_support::temp_home().unwrap();
    let (local, peer, remote) = enrolled_hosts(fixture.path(), &[]);
    run_sync(&local, &["--include-memories"]);
    run_sync(&peer, &[]);
    let committed = local.head();

    peer.work(&["remember", "Peer revision", "--key", "rule"]);
    run_sync(&peer, &["--no-push"]);
    peer.work(&["forget", "rule"]);
    run_sync(&peer, &["--no-push"]);
    let relative = "memories/rule/head.json";
    let tombstone = fs::read(peer.folder().join(relative)).unwrap();
    let document: Value = serde_json::from_slice(&tombstone).unwrap();
    assert_eq!(document["head"]["state"], "tombstone");

    local.work(&["remember", "Local rule", "--key", "rule"]);
    let loose = local.folder().join(relative);
    fs::create_dir_all(loose.parent().unwrap()).unwrap();
    fs::write(&loose, &tombstone).unwrap();
    let exclude = local.checkout().join(".git/info/exclude");
    let mut rules = fs::read_to_string(&exclude).unwrap_or_default();
    rules.push_str("\n/.engram/memories/rule/\n");
    fs::write(exclude, rules).unwrap();
    assert_eq!(
        data_worktree::git(
            &local.data(),
            &["status", "--porcelain", "--untracked-files=all"]
        ),
        b""
    );

    for _ in 0..2 {
        let output = ordinary_sync_output(&local, &["--no-push"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{stderr}");
        assert!(
            stderr.contains("memory_ignore_policy_excludes_tree"),
            "{stderr}"
        );
        assert_eq!(memory_body(&local, "rule"), "Local rule");
        assert_eq!(local.head(), committed);
        assert_eq!(remote_tip(&remote), committed);
    }
    assert_eq!(fs::read(loose).unwrap(), tombstone);
}

#[test]
fn one_command_refetches_after_a_rejected_push() {
    let fixture = test_support::temp_home().unwrap();
    let root = fixture.path();
    let (local, peer, remote) = enrolled_hosts(root, &[]);
    local.git(&["remote", "set-url", "origin", "../data.git"]);
    let seed = local.head();
    peer.work(&["add", "Peer task"]);
    peer.work(&["remember", "Peer rule", "--key", "peer-rule"]);
    let prepared = run_sync(&peer, &["--no-push", "--include-memories"]);
    assert_eq!(prepared["pushed"], false);
    let peer_tip = peer.head();
    assert_ne!(peer_tip, seed);
    let fixture_script = root.join("receive-pack.cjs");
    fs::write(&fixture_script, include_str!("receive-pack.cjs")).unwrap();
    fs::write(
        root.join("incoming.json"),
        serde_json::to_vec(&serde_json::json!({
            "checkout": peer.data(),
            "commit": peer_tip,
        }))
        .unwrap(),
    )
    .unwrap();
    let script_path = fixture_script
        .to_str()
        .unwrap()
        .replace('\\', "/")
        .replace('\'', "'\\''");
    let receiver = format!("node '{script_path}'");
    local.git(&["config", "--local", "remote.origin.receivepack", &receiver]);

    local.work(&["add", "Local task"]);
    local.work(&["remember", "Local rule", "--key", "local-rule"]);
    let result = run_sync(&local, &["--include-memories"]);
    assert_eq!(result["pushed"], true);
    assert_eq!(
        fs::read_to_string(root.join("receive-count"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    let published = data_worktree::git(&remote, &["rev-parse", "refs/heads/engram-data"]);
    assert_eq!(String::from_utf8(published).unwrap().trim(), local.head());
    let snapshot = engram::task_sync::git::read_task_snapshot(
        &remote,
        &ProjectId(PROJECT.into()),
        &local.head(),
    )
    .unwrap();
    assert!(snapshot.commit.parents.contains(&peer_tip));
    let titles = snapshot
        .tree
        .tasks
        .iter()
        .map(|task| task.title.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(titles, ["Local task", "Peer task", "Shared task"].into());
    let applied = snapshot
        .tree
        .tasks
        .iter()
        .find(|task| task.title == "Peer task")
        .unwrap();
    assert_eq!(local.title(applied.id), "Peer task");
    let local_task = snapshot
        .tree
        .tasks
        .iter()
        .find(|task| task.title == "Local task")
        .unwrap();
    let pulled = run_sync(&peer, &[]);
    assert_eq!(pulled["pushed"], false);
    assert_eq!(peer.head(), local.head());
    assert_eq!(peer.title(local_task.id), "Local task");
    assert_eq!(memory_body(&local, "peer-rule"), "Peer rule");
    assert_eq!(memory_body(&peer, "local-rule"), "Local rule");
}

#[test]
fn one_command_refuses_a_different_notes_profile_before_merging() {
    let fixture = test_support::temp_home().unwrap();
    let (local, peer, remote) = enrolled_hosts(fixture.path(), &[]);
    let seed = local.head();
    let peer_envelope = peer.folder().join("project.json");
    let mut basis: Value = serde_json::from_slice(&fs::read(&peer_envelope).unwrap()).unwrap();
    basis["project"]["notes_included"] = true.into();
    basis["profile"]["include_notes"] = true.into();
    basis["profile"]["origin_machine"] = "peer".into();
    fs::write(peer_envelope, serde_json::to_vec_pretty(&basis).unwrap()).unwrap();
    peer.commit("Change the notes profile");
    peer.git(&["push", "origin", "HEAD:refs/heads/engram-data"]);

    local.work(&["add", "Local task"]);
    let output = ordinary_sync_output(&local, &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("uses a different notes profile"),
        "{stderr}"
    );
    assert_ne!(local.head(), seed, "the local export was not committed");
    let parents = data_worktree::git(&local.data(), &["show", "-s", "--format=%P", "HEAD"]);
    assert_eq!(String::from_utf8(parents).unwrap().trim(), seed);
    let published = data_worktree::git(&remote, &["rev-parse", "refs/heads/engram-data"]);
    assert_eq!(String::from_utf8(published).unwrap().trim(), peer.head());
}

#[test]
fn one_command_preserves_an_ignored_file_when_tips_diverge() {
    let fixture = test_support::temp_home().unwrap();
    let (local, peer, remote) = enrolled_hosts(fixture.path(), &[]);
    let seed = local.head();
    let policy_path = local.folder().join(".gitattributes");
    assert!(!policy_path.exists());
    fs::write(peer.folder().join(".gitattributes"), "# remote policy\n").unwrap();
    peer.commit("Add incoming attributes");
    peer.git(&["push", "origin", "HEAD:refs/heads/engram-data"]);

    let exclude = local.checkout().join(".git/info/exclude");
    fs::create_dir_all(exclude.parent().unwrap()).unwrap();
    let mut rules = fs::read_to_string(&exclude).unwrap_or_default();
    rules.push_str("\n/.engram/.gitattributes\n");
    fs::write(exclude, rules).unwrap();
    let contents = "# private policy\n";
    fs::write(&policy_path, contents).unwrap();
    local.git(&["check-ignore", "--", ".engram/.gitattributes"]);

    local.work(&["add", "Local task"]);
    let output = ordinary_sync_output(&local, &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("set aside ignored local files under .engram"),
        "{stderr}"
    );
    assert_eq!(fs::read_to_string(policy_path).unwrap(), contents);
    let parents = data_worktree::git(&local.data(), &["show", "-s", "--format=%P", "HEAD"]);
    assert_eq!(String::from_utf8(parents).unwrap().trim(), seed);
    let published = data_worktree::git(&remote, &["rev-parse", "refs/heads/engram-data"]);
    assert_eq!(String::from_utf8(published).unwrap().trim(), peer.head());
}

#[test]
fn one_command_accepts_a_local_alias_after_a_notes_enabled_import() {
    let fixture = test_support::temp_home().unwrap();
    let (local, peer, _) = enrolled_hosts(fixture.path(), &NOTES_A);
    let seed = peer.head();
    assert_eq!(seed, local.head());

    let output = ordinary_sync_output(&peer, &["--no-push"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("with --origin-machine"), "{stderr}");
    assert_eq!(peer.head(), seed);

    peer.work(&["add", "Peer task"]);
    let prepared = run_sync(&peer, &["--origin-machine", "host-b", "--no-push"]);
    assert_eq!(prepared["pushed"], false);
    assert_ne!(peer.head(), seed);
    let envelope = fs::read(peer.folder().join("project.json")).unwrap();
    let basis: Value = serde_json::from_slice(&envelope).unwrap();
    assert_eq!(basis["profile"]["origin_machine"], "host-b");

    let first_export = peer.head();
    let resumed = run_sync(&peer, &["--no-push"]);
    assert_eq!(resumed["pushed"], false);
    assert_eq!(peer.head(), first_export);
}
