use super::*;

fn run(fixture: &Fixture, request: &Path) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_engram"));
    isolate_git(&mut command, &fixture.global_config);
    command
        .env("TOKIO_WORKER_THREADS", "1")
        .current_dir(&fixture.code)
        .arg("--home")
        .arg(&fixture.home)
        .arg("--project-file")
        .arg(fixture.code.join(".engram-project"))
        .args([
            "task-sync",
            "--actor-id",
            "fixture-operator",
            "--session-id",
            "fixture-session",
            "data-commit",
            "--branch",
            "ordinary",
            "--request",
        ])
        .arg(format!("@{}", request.display()))
        .output()
        .unwrap()
}

fn success(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn data_commit_retains_exact_staged_bytes_and_replays_without_advancing_either_branch() {
    let fixture = Fixture::new(false);
    fs::create_dir_all(fixture.root.join(".engram")).unwrap();
    let project = serde_json::json!({
        "format": engram::task_sync::FORMAT,
        "project_id": "fixture-project",
        "notes_included": false,
    });
    fs::write(
        fixture.root.join(".engram/project.json"),
        serde_json::to_vec(&project).unwrap(),
    )
    .unwrap();
    git(&fixture.root, &["add", "--", ".engram"]);
    let selected = |args: &[&str]| {
        String::from_utf8(git(&fixture.root, args))
            .unwrap()
            .trim()
            .to_owned()
    };
    let data_head = selected(&["rev-parse", "HEAD"]);
    let tree = selected(&["write-tree"]);
    let code_head = git(&fixture.code, &["rev-parse", "HEAD"]);
    let code_index = fs::read(fixture.common.join("index")).unwrap();
    let data_index = fs::read(fixture.admin.join("index")).unwrap();
    let request = engram::task_sync::data_worktree::StagedCommitRequest {
        operation: uuid::Uuid::new_v4(),
        tree: tree.clone(),
        expected_head: Some(data_head.clone()),
        parents: vec![data_head.clone()],
        author_name: "Fixture".into(),
        author_email: "fixture@example.invalid".into(),
        authored_at: chrono::DateTime::from_timestamp(chrono::Utc::now().timestamp(), 0).unwrap(),
        message: "Retain selected metadata\n".into(),
    };
    let request_file = fixture.owned.path().join("selected-commit.json");
    fs::write(&request_file, serde_json::to_vec(&request).unwrap()).unwrap();
    let first = success(&run(&fixture, &request_file));
    assert_eq!(success(&run(&fixture, &request_file)), first);
    let oid = first["oid"].as_str().unwrap();
    assert_eq!(
        selected(&["rev-parse", first["retention_ref"].as_str().unwrap()]),
        oid
    );
    assert_eq!(selected(&["rev-parse", &format!("{oid}^{{tree}}")]), tree);
    assert_eq!(selected(&["rev-parse", &format!("{oid}^")]), data_head);
    assert_eq!(selected(&["rev-parse", "HEAD"]), data_head);
    assert_eq!(git(&fixture.code, &["rev-parse", "HEAD"]), code_head);
    assert_eq!(fs::read(fixture.common.join("index")).unwrap(), code_index);
    assert_eq!(fs::read(fixture.admin.join("index")).unwrap(), data_index);
    let mut changed = request;
    changed.message.push_str("Changed selection\n");
    fs::write(&request_file, serde_json::to_vec(&changed).unwrap()).unwrap();
    let refused = run(&fixture, &request_file);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("data commit request differs from its retained selection")
    );
    assert_eq!(
        selected(&["rev-parse", first["retention_ref"].as_str().unwrap()]),
        oid
    );
    assert_eq!(selected(&["rev-parse", "HEAD"]), data_head);
    task_sync_opt_in::assert_no_database(&fixture.home);
}

#[test]
fn data_commit_opt_out_refuses_before_reading_a_request_file() {
    let fixture = Fixture::new(false);
    let database =
        engram::project_database_path(&fixture.home, &engram::ProjectId("fixture-project".into()));
    fs::remove_file(database.parent().unwrap().join("task-sync-setting.json")).unwrap();
    let output = run(&fixture, &fixture.owned.path().join("missing-request.json"));
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("task sync is not enabled"));
    task_sync_opt_in::assert_no_database(&fixture.home);
}
