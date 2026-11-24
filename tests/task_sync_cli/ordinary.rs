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

#[test]
fn one_command_refetches_after_a_rejected_push() {
    let fixture = test_support::temp_home().unwrap();
    let root = fixture.path();
    let (local, peer, remote) = enrolled_hosts(root, &[]);
    local.git(&["remote", "set-url", "origin", "../data.git"]);
    let seed = local.head();
    peer.work(&["add", "Peer task"]);
    let prepared = run_sync(&peer, &["--no-push"]);
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
    let result = run_sync(&local, &[]);
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
