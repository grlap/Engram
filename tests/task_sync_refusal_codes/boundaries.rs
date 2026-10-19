use super::*;
use serde_json::json;

#[test]
fn task_sync_export_missing_parent_keeps_io_even_with_json() {
    let fixture = Fixture::new();
    let source = fixture.store("source");
    fixture.add(&source, "shared task");
    let relative = Path::new("missing/candidate");
    for json in [false, true] {
        let output = fixture
            .sync_command(&source, "export", relative, json)
            .current_dir(&fixture.root)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(output.stdout.as_slice(), b"");
        let error = stderr(&output);
        assert_ne!(error, "");
        assert!(!error.contains("task_sync_"), "{error}");
        assert!(
            !error.contains("{\n"),
            "unexpected structured refusal: {error}"
        );
        assert!(!fixture.root.join("missing").exists());
    }

    // The same relative spelling works once its ordinary parent exists.
    fs::create_dir(fixture.root.join("missing")).unwrap();
    for _ in 0..2 {
        let output = fixture
            .sync_command(&source, "export", relative, true)
            .current_dir(&fixture.root)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(receipt["directory"], "missing/candidate");
    }
    assert_eq!(
        fs::read_dir(fixture.root.join("missing")).unwrap().count(),
        1
    );
}

#[test]
fn task_sync_json_oversized_note_keeps_byte_details_without_a_work_remedy() {
    use engram::task_sync::{
        SharedActor,
        notes::{SharedNote, SharedNoteKind},
        progress::ExportProfile,
    };
    let fixture = Fixture::new();
    let limit = 64 * 1024;
    let body = "Ω".repeat(limit / "Ω".len());
    // A self-consistent folder carrying a note at the limit.
    let mut folder = directory::read_folder(&fixture.candidate("plain")).unwrap();
    folder.profile = ExportProfile {
        include_notes: true,
        origin_machine: Some("machine-A".into()),
    };
    folder.tree.project.notes_included = true;
    folder.tree.tasks[0].notes = vec![SharedNote {
        id: engram::CanonicalObject::mint(&"shared note")
            .unwrap()
            .key()
            .clone(),
        kind: SharedNoteKind::Generic,
        body: body.clone(),
        author: SharedActor {
            actor_id: "writer".into(),
            actor_kind: "agent".into(),
        },
        recorded_at: "1970-01-01T00:00:01Z".parse().unwrap(),
        origin_machine: "machine-A".into(),
    }];
    let valid = fixture.root.join("valid-notes");
    directory::write_folder(&valid, &folder.tree, &folder.profile).unwrap();
    // The same folder with one byte too many in the note body.
    let candidate = fixture.root.join("notes");
    fs::create_dir_all(candidate.join("tasks")).unwrap();
    fs::copy(valid.join("project.json"), candidate.join("project.json")).unwrap();
    let task_name = format!("{}.json", folder.tree.tasks[0].id.0);
    let mut task: Value =
        serde_json::from_slice(&fs::read(valid.join("tasks").join(&task_name)).unwrap()).unwrap();
    task["notes"][0]["body"] = json!(format!("{body}x"));
    fs::write(
        candidate.join("tasks").join(&task_name),
        serde_json::to_vec(&task).unwrap(),
    )
    .unwrap();
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
    assert_imports_once(&fixture, &target, &valid);
}

/// The folder lands, and its exact repeat replays the same receipt.
fn assert_imports_once(fixture: &Fixture, target: &Path, folder: &Path) {
    let receipts = [0, 1].map(|_| {
        let output = fixture.sync(target, "import", folder, true);
        assert!(output.status.success(), "{}", stderr(&output));
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    });
    assert_eq!(receipts[0]["changed"], true);
    assert_eq!(receipts[1], receipts[0]);
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
    assert_imports_once(&fixture, &target, &candidate);
}
