use super::*;

fn run_sync(host: &Host, extra: &[&str]) -> Value {
    let output = command(&host.home, &host.marker)
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
        .unwrap();
    assert!(
        output.status.success(),
        "sync {extra:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn one_command_refetches_after_a_rejected_push() {
    let fixture = test_support::temp_home().unwrap();
    let root = fixture.path();
    let local = Host::new(root, "local");
    let peer = Host::new(root, "peer");
    let remote = root.join("data.git");
    fs::create_dir(&remote).unwrap();
    data_worktree::git(&remote, &["init", "--bare"]);
    for host in [&local, &peer] {
        host.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
    }

    local.work(&["add", "Shared task"]);
    assert_eq!(local.receipt("export", &[])["route"], "bootstrap");
    local.commit("Seed shared tasks");
    local.receipt(
        "import",
        &[
            "--basis",
            local.folder().join("project.json").to_str().unwrap(),
        ],
    );
    let seed = local.head();
    local.git(&["push", "origin", "HEAD:refs/heads/engram-data"]);
    peer.merge_from(&local);
    peer.receipt("import", &["--empty-basis"]);

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
}
