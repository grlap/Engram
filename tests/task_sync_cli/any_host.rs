//! Actual CLI execution and reported outcomes across isolated host stores.

use super::*;

fn initialize(home: &Path, marker: &Path, project: &str) {
    task_sync_opt_in::enable(home, project);
    let output = command(home, marker).arg("init").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn tree(folder: &Path) -> engram::task_sync::TaskTree {
    directory::read_folder(folder).unwrap().tree
}

fn export(home: &Path, marker: &Path, folder: &Path) {
    exchange_snapshot(home, marker, "export", folder, &["--json"]);
}

fn import(home: &Path, marker: &Path, folder: &Path, own: &Path) -> Value {
    exchange_snapshot(home, marker, "import", folder, &["--basis", &basis(own)])
}

fn commit_data(marker: &Path, subject: &str) -> String {
    let data = marker.parent().unwrap().join(".engram-data");
    data_worktree::git(&data, &["add", "-A", "--", ".engram"]);
    data_worktree::git(
        &data,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-m",
            subject,
        ],
    );
    String::from_utf8(data_worktree::git(&data, &["rev-parse", "HEAD"]))
        .unwrap()
        .trim()
        .into()
}

#[test]
fn task_sync_cli_partial_import_stops_base_advancement_and_preserves_held_remote_edits() {
    let fixture = test_support::temp_home().unwrap();
    let root = fixture.path();
    let marker = root.join(".engram-project");
    let project = "partial-import-cli";
    fs::write(&marker, format!("{project}\n")).unwrap();
    data_worktree::initialize(&marker);
    let [a, b] = ["a", "b"].map(|name| root.join(format!("home-{name}")));
    for home in [&a, &b] {
        initialize(home, &marker, project);
    }
    let empty = root.join("empty");
    export(&a, &marker, &empty);
    work(&a, &marker, &["add", "Held task"]);
    work(&a, &marker, &["add", "Independent task"]);
    let original = root.join("original");
    export(&a, &marker, &original);
    let initial = tree(&original);
    let held = initial
        .tasks
        .iter()
        .find(|task| task.title == "Held task")
        .unwrap()
        .id;
    let independent = initial
        .tasks
        .iter()
        .find(|task| task.title == "Independent task")
        .unwrap()
        .id;
    let held_ref = held.0.to_string();
    let independent_ref = independent.0.to_string();
    let common_commit = commit_data(&marker, "Publish original tasks");
    let caller_base = root.join("last-applied-commit");
    fs::write(&caller_base, &common_commit).unwrap();
    import(&b, &marker, &original, &empty);
    import(&a, &marker, &original, &original);
    work(&a, &marker, &["claim", &held_ref]);
    work(
        &b,
        &marker,
        &["update", &held_ref, "--title", "Held edit from B"],
    );
    work(
        &b,
        &marker,
        &[
            "update",
            &independent_ref,
            "--title",
            "Independent edit from B",
        ],
    );
    let incoming = root.join("incoming");
    export(&b, &marker, &incoming);
    let incoming_commit = commit_data(&marker, "Publish both edits from B");
    let database = project_database_path(&a, &ProjectId(project.into()));
    let before = feed_rows(&database, false);
    let dry = snapshot_output(
        &a,
        &marker,
        "import",
        &incoming,
        &["--basis", &basis(&original), "--dry-run", "--json"],
    );
    assert_eq!(dry.status.code(), Some(3));
    let preview: Value = serde_json::from_slice(&dry.stdout).unwrap();
    assert_eq!(preview["dry_run"], true);
    assert_eq!(feed_rows(&database, false), before);
    let output = snapshot_output(
        &a,
        &marker,
        "import",
        &incoming,
        &["--basis", &basis(&original), "--json"],
    );
    assert_eq!(output.status.code(), Some(3));
    let partial: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(partial["tasks"], preview["tasks"]);
    assert_eq!(partial["dry_run"], false);
    let tasks = partial["tasks"].as_array().unwrap();
    assert_eq!(
        tasks.iter().find(|task| task["task"] == held_ref).unwrap()["status"],
        "held"
    );
    assert_eq!(
        tasks
            .iter()
            .find(|task| task["task"] == independent_ref)
            .unwrap()["status"],
        "applied"
    );
    let guidance = String::from_utf8_lossy(&output.stderr);
    assert!(
        guidance.contains(
            &format!("tasks/{}.json", held.0).replace('/', std::path::MAIN_SEPARATOR_STR)
        ),
        "{guidance}"
    );
    assert!(guidance.contains("import again"), "{guidance}");
    // This is the wrapper's success branch: partial status must not enter it.
    if output.status.success() {
        fs::write(&caller_base, &incoming_commit).unwrap();
    }
    assert_eq!(fs::read_to_string(&caller_base).unwrap(), common_commit);
    let store = SqliteStore::open(&database).unwrap();
    assert_eq!(store.get_work_item(held).unwrap().title, "Held task");
    assert_eq!(
        store.get_work_item(independent).unwrap().title,
        "Independent edit from B"
    );
    drop(store);
    let live = data_worktree::folder(&marker);
    let pending_file = live.join(format!("tasks/{}.json", held.0));
    let pending_bytes = fs::read(&pending_file).unwrap();
    // An ordinary export leaves the held task's incoming bytes in place.
    let unchanged = sync_output(&a, &marker, "export", &live, &["--json"]);
    assert!(
        unchanged.status.success(),
        "{}",
        String::from_utf8_lossy(&unchanged.stderr)
    );
    let receipt: Value = serde_json::from_slice(&unchanged.stdout).unwrap();
    assert_eq!(receipt["route"], "unchanged");
    assert_eq!(fs::read(&pending_file).unwrap(), pending_bytes);
    work(
        &a,
        &marker,
        &[
            "update",
            &independent_ref,
            "--outcome",
            "New local outcome after the partial import",
        ],
    );
    // Preserve the incoming commit before preparing a fresh local basis.
    work(
        &a,
        &marker,
        &[
            "update",
            &held_ref,
            "--release",
            "--reason",
            "Yield to the incoming edit",
        ],
    );
    let fresh_basis = root.join("fresh-basis");
    export(&a, &marker, &fresh_basis);
    let data = marker.parent().unwrap().join(".engram-data");
    data_worktree::git(
        &data,
        &[
            "restore",
            "--source",
            &incoming_commit,
            "--staged",
            "--worktree",
            "--",
            ".engram",
        ],
    );
    assert_eq!(fs::read(&pending_file).unwrap(), pending_bytes);
    // The agent re-merges the retained commit against the fresh local export.
    // B did not change this outcome from the common commit, so A's newer
    // outcome survives alongside B's title and the pending held-task edit.
    let independent_file = live.join("tasks").join(format!("{}.json", independent.0));
    let mut resolved: Value =
        serde_json::from_slice(&fs::read(&independent_file).unwrap()).unwrap();
    assert_eq!(
        resolved["outcome"],
        initial
            .tasks
            .iter()
            .find(|task| task.id == independent)
            .unwrap()
            .outcome
    );
    let local = tree(&fresh_basis);
    resolved["outcome"] = Value::String(
        local
            .tasks
            .iter()
            .find(|task| task.id == independent)
            .unwrap()
            .outcome
            .clone(),
    );
    fs::write(
        &independent_file,
        serde_json::to_vec_pretty(&resolved).unwrap(),
    )
    .unwrap();
    let resolved_commit = commit_data(&marker, "Merge pending edits with newer local planning");
    let retried = sync_output(
        &a,
        &marker,
        "import",
        &live,
        &["--basis", &basis(&fresh_basis), "--json"],
    );
    assert!(
        retried.status.success(),
        "{}",
        String::from_utf8_lossy(&retried.stderr)
    );
    fs::write(&caller_base, &resolved_commit).unwrap();
    let returned = root.join("returned");
    export(&a, &marker, &returned);
    import(&b, &marker, &returned, &incoming);
    let store = SqliteStore::open(project_database_path(&b, &ProjectId(project.into()))).unwrap();
    assert_eq!(store.get_work_item(held).unwrap().title, "Held edit from B");
    let independent = store.get_work_item(independent).unwrap();
    assert_eq!(independent.title, "Independent edit from B");
    assert_eq!(
        independent.outcome,
        "New local outcome after the partial import"
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn task_sync_cli_any_host_execution_and_three_host_chain_preserve_outcomes_and_conflicts() {
    let fixture = test_support::temp_home().unwrap();
    let root = fixture.path();
    let marker = root.join(".engram-project");
    let project = "any-host-chain-cli";
    fs::write(&marker, format!("{project}\n")).unwrap();
    data_worktree::initialize(&marker);
    let [a, b, c] = ["a", "b", "c"].map(|host| root.join(format!("home-{host}")));
    for home in [&a, &b, &c] {
        initialize(home, &marker, project);
    }

    let empty = root.join("empty");
    export(&a, &marker, &empty);
    for home in [&a, &b, &c] {
        exchange_snapshot(home, &marker, "import", &empty, &["--empty-basis"]);
    }
    work(
        &a,
        &marker,
        &[
            "add",
            "Created on A, executed on B",
            "--accept",
            "Return the outcome",
        ],
    );
    work(
        &a,
        &marker,
        &[
            "add",
            "Chain planning",
            "--accept",
            "Carry changes from every host",
        ],
    );
    let a0 = root.join("a0");
    export(&a, &marker, &a0);
    let initial = tree(&a0);
    let executed = initial
        .tasks
        .iter()
        .find(|task| task.title.starts_with("Created on A"))
        .unwrap()
        .id;
    let chain = initial
        .tasks
        .iter()
        .find(|task| task.title == "Chain planning")
        .unwrap()
        .id;
    let executed_ref = executed.0.to_string();
    let chain_ref = chain.0.to_string();
    import(&b, &marker, &a0, &empty);

    // B works while disconnected from A; every operation uses the CLI.
    work(
        &b,
        &marker,
        &["update", &chain_ref, "--after", &executed_ref],
    );
    work(
        &b,
        &marker,
        &["update", &chain_ref, "--title", "B changed the chain title"],
    );
    work(&b, &marker, &["claim", &executed_ref]);
    work(&b, &marker, &["note", "Implemented while disconnected"]);
    work(
        &b,
        &marker,
        &["done", &executed_ref, "Delivered and checked on B"],
    );
    let b1 = root.join("b1");
    export(&b, &marker, &b1);
    assert_eq!(
        tree(&b1)
            .tasks
            .iter()
            .find(|task| task.id == executed)
            .unwrap()
            .lifecycle,
        engram::WorkLifecycle::Completed
    );

    let direct = import(&a, &marker, &b1, &a0);
    assert_eq!(direct["changed"], true);
    let a_direct = root.join("a-direct-return");
    export(&a, &marker, &a_direct);
    assert_eq!(
        tree(&a_direct),
        tree(&b1),
        "A→B→A returns the executed outcome"
    );

    // C relays B's outcome and contributes its own planning edit and child.
    import(&c, &marker, &b1, &empty);
    work(
        &c,
        &marker,
        &["update", &chain_ref, "--outcome", "Outcome authored on C"],
    );
    work(
        &c,
        &marker,
        &["add", "Child authored on C", "--under", &chain_ref],
    );
    let c1 = root.join("c1");
    export(&c, &marker, &c1);
    let returned = import(&a, &marker, &c1, &a_direct);
    assert_eq!(returned["changed"], true);
    assert!(
        returned["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|task| task["status"] == "applied")
    );
    let a1 = root.join("a1");
    export(&a, &marker, &a1);
    assert_eq!(tree(&a1), tree(&c1));
    let shared = tree(&a1);
    let completion = shared
        .tasks
        .iter()
        .find(|task| task.id == executed)
        .unwrap()
        .completion
        .as_ref()
        .unwrap();
    assert!(completion.summary.contains("Delivered and checked on B"));
    let planning = shared.tasks.iter().find(|task| task.id == chain).unwrap();
    assert_eq!(planning.title, "B changed the chain title");
    assert_eq!(planning.outcome, "Outcome authored on C");
    assert_eq!(planning.prerequisites, [executed]);
    assert!(
        shared
            .tasks
            .iter()
            .any(|task| task.parent_id == Some(chain) && task.title == "Child authored on C")
    );
    let database = project_database_path(&a, &ProjectId(project.into()));
    let store = SqliteStore::open(&database).unwrap();
    let item = store.get_work_item(executed).unwrap();
    assert!(item.restored && item.active_run_id.is_none());
    let health = store.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
    drop(store);
    let connection = rusqlite::Connection::open(&database).unwrap();
    let seals: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM work_completion_seals WHERE work_id=?1",
            [&executed_ref],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        seals, 0,
        "B's outcome confers no local completion proof on A"
    );
    drop(connection);
    let before = feed_rows(&database, false);
    import(&a, &marker, &c1, &a_direct);
    assert_eq!(
        feed_rows(&database, false),
        before,
        "a relayed outcome applies exactly once"
    );

    // Both sides begin from the same shared state. B's concurrent edit only
    // reaches A after C has imported and relayed it.
    import(&b, &marker, &a1, &b1);
    let bbase = root.join("bbase");
    export(&b, &marker, &bbase);
    assert_eq!(tree(&bbase), tree(&a1));
    work(
        &a,
        &marker,
        &["update", &chain_ref, "--title", "A concurrent title"],
    );
    work(
        &b,
        &marker,
        &["update", &chain_ref, "--title", "B concurrent title"],
    );
    let b2 = root.join("b2");
    export(&b, &marker, &b2);
    import(&c, &marker, &b2, &c1);
    let c2 = root.join("c2");
    export(&c, &marker, &c2);
    let output = snapshot_output(
        &a,
        &marker,
        "import",
        &c2,
        &["--basis", &basis(&a1), "--json"],
    );
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error"]["code"], "task_sync_divergent_planning");
    let detail = error["error"]["message"].as_str().unwrap();
    assert!(
        detail.contains("A concurrent title") && detail.contains("B concurrent title"),
        "{detail}"
    );
    let store = SqliteStore::open(&database).unwrap();
    assert_eq!(
        store.get_work_item(chain).unwrap().title,
        "A concurrent title"
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

// Two hosts that planned and executed work before ever exchanging: no
// baseline, begun feeds and a live claim each. Each joins the exchange by
// importing the merged folder against its own export from before the merge.
#[test]
fn task_sync_cli_native_hosts_without_a_baseline_enrol_with_their_own_basis_both_ways() {
    let fixture = test_support::temp_home().unwrap();
    let root = fixture.path();
    let marker = root.join(".engram-project");
    let project = "native-enrol-cli";
    fs::write(&marker, format!("{project}\n")).unwrap();
    data_worktree::initialize(&marker);
    let hosts = [("A", root.join("home-a")), ("B", root.join("home-b"))];
    let exported = |home: &Path, name: &str| {
        let folder = root.join(name);
        export(home, &marker, &folder);
        folder
    };
    let row = |home: &Path, id: &str| -> Vec<rusqlite::types::Value> {
        let database = project_database_path(home, &ProjectId(project.into()));
        rusqlite::Connection::open(database)
            .unwrap()
            .query_row(
                "SELECT item_json, latest_event_id, active_run_id FROM work_items
                 WHERE work_id = ?1",
                [id],
                |row| (0..3_usize).map(|column| row.get(column)).collect(),
            )
            .unwrap()
    };
    let mut local = Vec::new();
    for (host, home) in &hosts {
        initialize(home, &marker, project);
        work(home, &marker, &["add", &format!("Executed on {host}")]);
        work(home, &marker, &["add", &format!("Planned on {host}")]);
        let probe = exported(home, &format!("probe-{host}"));
        let executed = tree(&probe)
            .tasks
            .iter()
            .find(|task| task.title == format!("Executed on {host}"))
            .unwrap()
            .id
            .0
            .to_string();
        work(home, &marker, &["claim", &executed]);
        assert_ne!(row(home, &executed)[2], rusqlite::types::Value::Null);
        let own = exported(home, &format!("own-{host}"));
        let ids = tree(&own)
            .tasks
            .iter()
            .map(|task| task.id.0.to_string())
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), 2);
        local.push((own, ids));
    }
    let mut merged_tree = tree(&local[0].0);
    merged_tree.tasks.extend(tree(&local[1].0).tasks);
    merged_tree.tasks.sort_by_key(|task| task.id.0);
    let merged = root.join("merged");
    directory::write_folder(
        &merged,
        &merged_tree,
        &ExportProfile {
            include_notes: false,
            origin_machine: None,
        },
    )
    .unwrap();

    for (index, (host, home)) in hosts.iter().enumerate() {
        let (own, ids) = &local[index];
        let foreign = &local[1 - index].1;
        let before = ids.iter().map(|id| row(home, id)).collect::<Vec<_>>();
        let enrolled = import(home, &marker, &merged, own);
        assert_eq!(enrolled["changed"], true, "{host}: {enrolled}");
        let mut applied = enrolled["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|task| {
                assert_eq!(task["status"], "applied", "{host}: {task}");
                task["task"].as_str().unwrap().to_owned()
            })
            .collect::<Vec<_>>();
        applied.sort();
        let mut expected = foreign.clone();
        expected.sort();
        assert_eq!(
            applied, expected,
            "{host} adds exactly the other host's tasks"
        );
        assert_eq!(
            ids.iter().map(|id| row(home, id)).collect::<Vec<_>>(),
            before,
            "{host}'s own tasks and their execution state are unchanged"
        );
        let after = exported(home, &format!("after-{host}"));
        assert_eq!(tree(&after).tasks, merged_tree.tasks);
        let again = import(home, &marker, &merged, &after);
        assert_eq!(again["changed"], false, "{host}: {again}");
        let store =
            SqliteStore::open(project_database_path(home, &ProjectId(project.into()))).unwrap();
        assert!(store.verify_all().unwrap().is_healthy());
    }
}
