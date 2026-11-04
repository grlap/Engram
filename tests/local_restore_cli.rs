//! Both CLI restore and diagnostic readers use the shared typed occurrence.

#[path = "../src/test_support.rs"]
mod test_support;

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use engram::{
    ProjectId, SqliteStore,
    backup::{
        CopyKind,
        restore::{
            RestoreRecord, RestoreRecords, RestoreSource, RestoreState, read_restore_record,
            restore_record_path, write_restore_record,
        },
        target::{PushLock, RecordPaths},
    },
};
use serde_json::Value;

struct Fixture {
    root: test_support::TempHome,
    project: ProjectId,
    home: PathBuf,
    database: PathBuf,
    source: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = test_support::temp_home().unwrap();
        let project = ProjectId("local-restore-cli".into());
        fs::write(root.path().join(".engram-project"), &project.0).unwrap();
        let home = root.path().join("home");
        let database = engram::project_database_path(&home, &project);
        let source = root.path().join("source.db");
        let store = SqliteStore::open(root.path().join("origin.db")).unwrap();
        store.backup_to(&source).unwrap();
        drop(store);
        Self {
            root,
            project,
            home,
            database,
            source,
        }
    }

    fn command(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_engram"))
            .current_dir(self.root.path())
            .env_remove("ENGRAM_HOME")
            .env_remove("ENGRAM_SESSION_ID")
            .env_remove("ENGRAM_ACTOR_ID")
            .arg("--home")
            .arg(&self.home)
            .args(args)
            .output()
            .unwrap()
    }

    fn success(&self, args: &[&str]) -> Value {
        let output = self.command(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn record(&self) -> RestoreRecord {
        let RestoreRecords::Recorded(record) = read_restore_record(&self.home, &self.project)
        else {
            panic!("restore record")
        };
        *record
    }
}

#[test]
fn local_restore_preserves_manifest_output_and_records_each_occurrence() {
    let fixture = Fixture::new();
    let from = fixture.source.to_str().unwrap();
    let first = fixture.success(&["restore", "--from", from]);
    let record = fixture.record();
    assert_eq!(record.state, RestoreState::Completed);
    assert_eq!(first["file_sha256"], record.sha256);
    assert_eq!(first["path"], fixture.database.to_str().unwrap());
    let RestoreSource::LocalFile { path, replace } = &record.source else {
        panic!("local provenance")
    };
    assert_eq!(
        Path::new(path),
        std::path::absolute(&fixture.source).unwrap()
    );
    assert!(!replace);
    assert!(
        !fixture
            .command(&["restore", "--from", from])
            .status
            .success()
    );
    assert_eq!(fixture.record(), record);
    let second = fixture.success(&["restore", "--from", from, "--replace"]);
    assert_eq!(first["file_sha256"], second["file_sha256"]);
    assert_ne!(fixture.record().occurrence_id, record.occurrence_id);
    assert_eq!(
        fs::read(&fixture.database).unwrap(),
        fs::read(&fixture.source).unwrap()
    );
    let self_restore = fixture.command(&[
        "restore",
        "--from",
        fixture.database.to_str().unwrap(),
        "--replace",
    ]);
    assert!(!self_restore.status.success());
    assert!(String::from_utf8_lossy(&self_restore.stderr).contains("the store itself"));
}

#[test]
fn local_abandonment_reinstates_the_completed_occurrence_without_changing_the_store() {
    let fixture = Fixture::new();
    let from = fixture.source.to_str().unwrap();
    fixture.success(&["restore", "--from", from]);
    let original = fixture.record();
    let before = fs::read(&fixture.database).unwrap();
    let blocker = rusqlite::Connection::open(&fixture.database).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let refused = fixture.command(&["restore", "--from", from, "--replace"]);
    assert!(!refused.status.success());
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(message.contains("--abandon-pending"), "{message}");
    let pending = fixture.record();
    fs::remove_file(&fixture.source).unwrap();
    let wrong = fixture.command(&[
        "restore",
        "--from",
        "wrong.db",
        "--abandon-pending",
        "--abandoned-by",
        "operator",
    ]);
    assert!(!wrong.status.success());
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("another source is pending"));
    assert_eq!(fixture.record(), pending);
    let abandoned = fixture.success(&[
        "restore",
        "--from",
        from,
        "--abandon-pending",
        "--abandoned-by",
        "operator",
    ]);
    assert_eq!(
        abandoned["occurrence_id"],
        pending.occurrence_id.to_string()
    );
    assert_eq!(abandoned["abandoned_by"], "operator");
    assert_eq!(abandoned["staging_removed"], true);
    let archive: Value =
        serde_json::from_slice(&fs::read(abandoned["archive"].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!(
        archive["pending"]["occurrence_id"],
        pending.occurrence_id.to_string()
    );
    assert_eq!(
        read_restore_record(&fixture.home, &fixture.project),
        RestoreRecords::Recorded(Box::new(original))
    );
    assert_eq!(fs::read(&fixture.database).unwrap(), before);
    drop(blocker);
}

#[test]
fn a_retry_announces_its_recorded_bytes_when_the_source_is_now_a_different_healthy_store() {
    let fixture = Fixture::new();
    let from = fixture.source.to_str().unwrap();
    fixture.success(&["restore", "--from", from]);
    let blocker = rusqlite::Connection::open(&fixture.database).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert!(
        !fixture
            .command(&["restore", "--from", from, "--replace"])
            .status
            .success()
    );
    let pending = fixture.record();
    drop(blocker);
    let source = rusqlite::Connection::open(&fixture.source).unwrap();
    source.execute_batch("VACUUM").unwrap();
    drop(source);
    let current = SqliteStore::verify_backup(&fixture.source).unwrap();
    assert_ne!(current.file_sha256, pending.sha256);
    let output = fixture.command(&["restore", "--from", from, "--replace"]);
    let notice = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{notice}");
    assert!(
        notice.contains(&pending.occurrence_id.to_string()),
        "{notice}"
    );
    assert!(notice.contains(&pending.pending_at.to_string()), "{notice}");
    assert!(notice.contains(&pending.sha256), "{notice}");
    assert!(notice.contains("earlier checked bytes"), "{notice}");
    assert!(
        notice.contains("not the source file's current bytes"),
        "{notice}"
    );
    let manifest: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(manifest["file_sha256"], pending.sha256);
    assert_eq!(fixture.record().occurrence_id, pending.occurrence_id);
    assert_eq!(
        SqliteStore::verify_backup(&fixture.database)
            .unwrap()
            .file_sha256,
        pending.sha256
    );
}

#[test]
fn local_provenance_status_and_doctor_take_no_writer_lock_and_change_no_record_or_data() {
    let fixture = Fixture::new();
    fixture.success(&["restore", "--from", fixture.source.to_str().unwrap()]);
    let mut record = fixture.record();
    let lock = PushLock::try_acquire(&RecordPaths::new(
        &fixture.home,
        &fixture.project,
        CopyKind::Store,
    ))
    .unwrap();
    for state in [RestoreState::Pending, RestoreState::Completed] {
        record.state = state;
        record.completed_at = (state == RestoreState::Completed).then(chrono::Utc::now);
        write_restore_record(&fixture.home, &fixture.project, &lock, &record).unwrap();
        let path = restore_record_path(&fixture.home, &fixture.project);
        let before_record = fs::read(&path).unwrap();
        let before_data = fs::read(&fixture.database).unwrap();
        let status = fixture.success(&["backup", "status", "--json"]);
        let doctor = fixture.success(&["doctor", "--json"]);
        assert_eq!(status["restore"]["record"]["source"]["kind"], "local_file");
        for observed in [&status["restore"], &doctor["backup"]["restore"]] {
            assert_eq!(
                observed["record"]["occurrence_id"],
                record.occurrence_id.to_string()
            );
            assert_eq!(
                observed["state"],
                if state == RestoreState::Pending {
                    "pending"
                } else {
                    "restored"
                }
            );
        }
        assert_eq!(fs::read(&path).unwrap(), before_record);
        assert_eq!(fs::read(&fixture.database).unwrap(), before_data);
        let plain = fixture.command(&["backup", "status"]);
        let text = String::from_utf8_lossy(&plain.stdout);
        assert!(text.contains("origin retirement not asserted"), "{text}");
        assert!(!text.contains("origin retired by"));
    }
    let path = restore_record_path(&fixture.home, &fixture.project);
    fs::write(&path, b"unreadable").unwrap();
    let before_data = fs::read(&fixture.database).unwrap();
    let status = fixture.success(&["backup", "status", "--json"]);
    let doctor = fixture.success(&["doctor", "--json"]);
    assert_eq!(status["restore"]["state"], "unreadable");
    assert_eq!(doctor["backup"]["restore"]["state"], "unreadable");
    assert_eq!(fs::read(path).unwrap(), b"unreadable");
    assert_eq!(fs::read(&fixture.database).unwrap(), before_data);
}
