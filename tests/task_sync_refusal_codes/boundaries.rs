use super::*;
use serde_json::json;

#[test]
fn task_sync_json_oversized_note_keeps_byte_details_without_a_work_remedy() {
    let fixture = Fixture::new();
    let candidate = fixture.candidate("notes");
    let project_path = candidate.join("project.json");
    let mut project: Value = serde_json::from_slice(&fs::read(&project_path).unwrap()).unwrap();
    project["notes_included"] = json!(true);
    fs::write(&project_path, serde_json::to_vec(&project).unwrap()).unwrap();
    let task_path = fs::read_dir(candidate.join("tasks"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mut task: Value = serde_json::from_slice(&fs::read(&task_path).unwrap()).unwrap();
    let limit = 64 * 1024;
    let body = "Ω".repeat(limit / "Ω".len());
    let id = engram::CanonicalObject::mint(&"shared note")
        .unwrap()
        .key()
        .clone();
    task["notes"] = json!([{
        "id": id, "kind": "generic", "body": format!("{body}x"),
        "author": { "actor_id": "writer", "actor_kind": "agent" },
        "recorded_at": "1970-01-01T00:00:01Z", "origin_machine": "machine-A"
    }]);
    fs::write(&task_path, serde_json::to_vec(&task).unwrap()).unwrap();
    let target = fixture.store("target");
    let refused = refusal(&fixture.sync(&target, "import", &candidate, true));
    assert_eq!(refused["error"]["code"], "task_sync_invalid_graph");
    assert_eq!(
        refused["error"]["details"],
        json!({
            "bytes": limit + 1, "limit": limit,
            "reason": "note body exceeds the UTF-8 byte limit"
        })
    );
    assert_plain_text_kept(
        &fixture.sync(&target, "import", &candidate, false),
        &refused,
    );
    task["notes"][0]["body"] = json!(body);
    fs::write(&task_path, serde_json::to_vec(&task).unwrap()).unwrap();
    for changed in [true, false] {
        let output = fixture.sync(&target, "import", &candidate, true);
        assert!(output.status.success(), "{}", stderr(&output));
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["changed"], changed);
    }
}

#[test]
fn task_sync_json_file_refusal_omits_the_plain_reading_path_context() {
    let fixture = Fixture::new();
    let candidate = fixture.candidate("candidate");
    let target = fixture.store("target");
    let path = candidate.join("project.json");
    let original = fs::read(&path).unwrap();
    fs::write(&path, vec![b' '; 1024 * 1024 + 1]).unwrap();
    let plain = fixture.sync(&target, "import", &candidate, false);
    assert!(!plain.status.success());
    assert!(stderr(&plain).contains(&format!("reading {}", path.display())));
    let refused = refusal(&fixture.sync(&target, "import", &candidate, true));
    assert_eq!(refused["error"]["code"], "task_sync_destination_file_limit");
    assert_eq!(
        refused["error"]["details"],
        json!({
            "reason": "task candidate file must be an ordinary bounded file"
        })
    );
    assert_eq!(
        refused["error"]["message"],
        "local work input is invalid: task candidate file must be an ordinary bounded file"
    );
    let encoded_path = serde_json::to_string(&path.to_string_lossy()).unwrap();
    // Compare JSON with JSON so Windows backslash escaping cannot hide a path.
    assert!(
        !refused
            .to_string()
            .contains(&encoded_path[1..encoded_path.len() - 1])
    );
    assert!(refused["error"].get("path").is_none());
    assert_plain_text_kept(&plain, &refused);
    fs::write(&path, original).unwrap();
    assert!(
        fixture
            .sync(&target, "import", &candidate, true)
            .status
            .success()
    );
}

#[cfg(windows)]
#[tokio::test]
async fn task_sync_real_locked_store_failure_keeps_its_generic_storage_code() {
    use std::time::Duration;
    let fixture = Fixture::new();
    let candidate = fixture.candidate("candidate");
    let target = fixture.store("target");
    let database = engram::project_database_path(&target, &engram::ProjectId(PROJECT.into()));
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection.execute_batch("BEGIN IMMEDIATE").unwrap();
    // The acquired transaction is the synchronization point. It stays owned
    // across the real CLI call; no sleep guesses when another process is ready.
    let mut command =
        tokio::process::Command::from(fixture.sync_command(&target, "import", &candidate, true));
    command.kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .expect("task-sync import must finish within its bounded SQLite wait and CLI watchdog")
        .unwrap();
    let refused = refusal(&output);
    assert_eq!(refused["error"]["code"], "engram_store_error");
    let sqlite_code = refused["error"]["details"]["sqlite_primary_code"].as_i64();
    let message = refused["error"]["message"].as_str().unwrap();
    assert!(
        sqlite_code == Some(i64::from(rusqlite::ffi::SQLITE_BUSY))
            || sqlite_code == Some(i64::from(rusqlite::ffi::SQLITE_LOCKED))
            || message.contains("database is locked"),
        "expected a SQLite busy/locked failure: {refused}"
    );
    assert!(
        !refused["error"]["code"]
            .as_str()
            .unwrap()
            .starts_with("task_sync_")
    );
    assert!(refused["error"]["details"].get("remedy").is_none());
    connection.execute_batch("ROLLBACK").unwrap();
    for changed in [true, false] {
        let output = fixture.sync(&target, "import", &candidate, true);
        assert!(output.status.success(), "{}", stderr(&output));
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["changed"], changed);
    }
}
