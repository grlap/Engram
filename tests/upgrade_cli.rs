#[path = "../src/test_support.rs"]
mod test_support;

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use engram::{
    ProjectId, SessionId,
    work_service::{LocalWorkService, WorkProposeInput},
};
use serde_json::Value;
use sha2::{Digest, Sha256};

fn add_root(path: &Path, title: &str, key: &str) {
    LocalWorkService::new(
        path.to_path_buf(),
        ProjectId("upgrade-cli-fixture".into()),
        "upgrade-test".into(),
        SessionId("upgrade-cli-fixture".into()),
        None,
    )
    .work_propose(
        WorkProposeInput::Root {
            evaluation_mode: None,
            external_ref: None,
            notes: Vec::new(),
            title: title.into(),
            outcome: "preserve complete committed work".into(),
            acceptance: vec!["survives upgrade".into()],
            work_kind: None,
            priority: Some(1),
            labels: Vec::new(),
            assigned_to: None,
            deferred_until: None,
            idempotency_key: key.into(),
        },
        chrono::Utc::now(),
    )
    .expect("fixture work");
}

// A child-process exit skips SQLite destructors and leaves committed WAL data.
// The parent owns the temporary directory; no process keeps a handle during upgrade.
#[test]
fn upgrade_fixture_leaves_committed_wal() {
    let Some(path) = std::env::var_os("ENGRAM_UPGRADE_TEST_WAL_SOURCE") else {
        return;
    };
    let source = PathBuf::from(path);
    add_root(&source, "Before WAL keeper", "before-keeper");
    let keeper = rusqlite::Connection::open(&source).expect("keeper");
    keeper
        .execute_batch("PRAGMA wal_autocheckpoint=0; BEGIN; SELECT count(*) FROM objects;")
        .expect("keep an old read snapshot open");
    add_root(&source, "Committed WAL survives upgrade", "committed-wal");
    assert!(fs::metadata(sidecar(&source, "-wal")).expect("WAL").len() > 0);
    std::process::exit(73);
}

fn sidecar(source: &Path, suffix: &str) -> PathBuf {
    let mut path = source.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

fn create_wal_source(source: &Path) {
    let output = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "upgrade_fixture_leaves_committed_wal",
            "--nocapture",
        ])
        .env("ENGRAM_UPGRADE_TEST_WAL_SOURCE", source)
        .output()
        .expect("fixture subprocess");
    assert_eq!(
        output.status.code(),
        Some(73),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::metadata(sidecar(source, "-wal"))
            .expect("retained WAL")
            .len()
            > 0
    );
}

#[test]
fn upgrade_fixture_leaves_empty_wal() {
    let Some(path) = std::env::var_os("ENGRAM_UPGRADE_TEST_EMPTY_WAL_SOURCE") else {
        return;
    };
    let source = PathBuf::from(path);
    add_root(&source, "Empty WAL original", "empty-wal");
    let keeper = rusqlite::Connection::open(&source).expect("keeper");
    keeper
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_checkpoint(TRUNCATE); SELECT count(*) FROM objects;")
        .expect("checkpoint-truncated WAL");
    assert_eq!(
        fs::metadata(sidecar(&source, "-wal"))
            .expect("empty WAL")
            .len(),
        0
    );
    assert!(sidecar(&source, "-shm").exists());
    std::process::exit(73);
}

// T1/C3: a stopped SQLite child can leave a present, zero-byte WAL and SHM.
#[test]
fn upgrade_cli_empty_wal_roundtrip_preserves_absence_and_empty() {
    for action in ["rollback", "finalize"] {
        let directory = test_support::temp_home().expect("fixture");
        let source = directory.path().join("source.db");
        let operation = directory.path().join("upgrade");
        let child = Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", "upgrade_fixture_leaves_empty_wal", "--nocapture"])
            .env("ENGRAM_UPGRADE_TEST_EMPTY_WAL_SOURCE", &source)
            .output()
            .expect("fixture subprocess");
        assert_eq!(
            child.status.code(),
            Some(73),
            "{}",
            String::from_utf8_lossy(&child.stderr)
        );
        let original = fs::read(&source).expect("main");
        assert_eq!(
            fs::read(sidecar(&source, "-wal")).expect("present WAL"),
            Vec::<u8>::new()
        );
        assert!(!sidecar(&source, "-journal").exists());
        success(&prepare(directory.path(), &source, &operation));
        success(&step(directory.path(), &operation, "activate"));
        let expected = if action == "rollback" {
            "rolled_back"
        } else {
            "finalized"
        };
        assert_eq!(
            success(&step(directory.path(), &operation, action))["phase"],
            expected
        );
        assert_eq!(
            success(&step(directory.path(), &operation, "recover"))["phase"],
            expected
        );
        if action == "rollback" {
            assert_eq!(fs::read(&source).expect("restored main"), original);
            assert_eq!(
                fs::read(sidecar(&source, "-wal")).expect("restored empty WAL"),
                Vec::<u8>::new()
            );
        } else {
            assert_eq!(
                fs::read(&source).expect("candidate live"),
                fs::read(operation.join("candidate.db")).expect("candidate")
            );
            assert!(!sidecar(&source, "-wal").exists());
        }
        assert!(!sidecar(&source, "-journal").exists());
    }
}

fn run(directory: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_engram"))
        .current_dir(directory)
        .arg("--home")
        .arg(directory.join("must-not-create-home"))
        .arg("migration")
        .args(args)
        .output()
        .expect("upgrade CLI")
}

fn success(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("JSON report")
}

fn objects(source: &Path) -> Vec<(String, Vec<u8>)> {
    let connection =
        rusqlite::Connection::open_with_flags(source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("read source");
    connection
        .prepare("SELECT object_hash, canonical_json FROM objects ORDER BY object_hash")
        .expect("objects query")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("objects")
        .collect::<Result<_, _>>()
        .expect("object bytes")
}

fn assert_wal_work(source: &Path) {
    let connection =
        rusqlite::Connection::open_with_flags(source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("inspect work without changing journal mode");
    let items: Vec<Vec<u8>> = connection
        .prepare("SELECT item_json FROM work_items")
        .expect("work query")
        .query_map([], |row| row.get(0))
        .expect("work titles")
        .collect::<Result<_, _>>()
        .expect("items");
    let mut titles: Vec<String> = items
        .iter()
        .map(|bytes| {
            serde_json::from_slice::<Value>(bytes).expect("stored work item")["title"]
                .as_str()
                .expect("authored title")
                .to_owned()
        })
        .collect();
    titles.sort();
    assert_eq!(
        titles,
        ["Before WAL keeper", "Committed WAL survives upgrade"]
    );
}

fn prepare(directory: &Path, source: &Path, operation: &Path) -> Output {
    run(
        directory,
        &[
            "prepare",
            "--database",
            source.to_str().expect("source path"),
            "--operation",
            operation.to_str().expect("operation path"),
            "--old-executable",
            env!("CARGO_BIN_EXE_engram"),
            "--offline-confirmed",
        ],
    )
}

fn step(directory: &Path, operation: &Path, action: &str) -> Output {
    if action == "status" {
        return run(
            directory,
            &[
                action,
                "--operation",
                operation.to_str().expect("operation path"),
            ],
        );
    }
    run(
        directory,
        &[
            action,
            "--operation",
            operation.to_str().expect("operation path"),
            "--offline-confirmed",
        ],
    )
}

fn refusal(output: &Output) {
    assert!(
        !output.status.success(),
        "unexpected success: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd)]
enum SnapshotEntry {
    Directory,
    File([u8; 32]),
    Symlink(PathBuf),
    Other,
}

fn directory_snapshot(root: &Path) -> Vec<(PathBuf, SnapshotEntry)> {
    fn visit(root: &Path, path: &Path, entries: &mut Vec<(PathBuf, SnapshotEntry)>) {
        for entry in fs::read_dir(path).expect("fixture inventory") {
            let path = entry.expect("entry").path();
            let relative = path
                .strip_prefix(root)
                .expect("owned fixture path")
                .to_path_buf();
            let metadata = fs::symlink_metadata(&path).expect("fixture metadata");
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                entries.push((
                    relative,
                    SnapshotEntry::Symlink(fs::read_link(&path).expect("link")),
                ));
                continue;
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if metadata.file_attributes() & 0x400 != 0 {
                    entries.push((relative, SnapshotEntry::Other));
                    continue;
                }
            }
            if file_type.is_dir() {
                entries.push((relative, SnapshotEntry::Directory));
                visit(root, &path, entries);
            } else if file_type.is_file() {
                entries.push((
                    relative,
                    SnapshotEntry::File(
                        Sha256::digest(fs::read(path).expect("fixture bytes")).into(),
                    ),
                ));
            } else {
                entries.push((relative, SnapshotEntry::Other));
            }
        }
    }
    let mut entries = Vec::new();
    visit(root, root, &mut entries);
    entries.sort();
    entries
}

fn append_rollback_intent(operation: &Path) {
    append_journal_transition(operation, "activated", "rolling_back");
}

fn append_journal_transition(operation: &Path, previous: &str, next: &str) {
    let mut records: Vec<_> = fs::read_dir(operation.join("journal"))
        .expect("journal")
        .map(|entry| entry.expect("entry").path())
        .collect();
    records.sort();
    let mut intent: Value =
        serde_json::from_slice(&fs::read(records.last().expect("last record")).expect("record"))
            .expect("record JSON");
    assert_eq!(intent["kind"], previous);
    let sequence = intent["sequence"].as_u64().expect("sequence") + 1;
    intent["sequence"] = sequence.into();
    intent["kind"] = next.into();
    fs::write(
        operation
            .join("journal")
            .join(format!("{sequence:08}.json")),
        serde_json::to_vec(&intent).expect("intent JSON"),
    )
    .expect("simulated interrupted transition");
}

#[test]
fn upgrade_cli_requires_offline_attestation_before_creating_artifacts() {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    let operation = directory.path().join("upgrade");
    drop(engram::SqliteStore::open_unresolved(&source).expect("source"));
    let before = fs::read(&source).expect("before");
    refusal(&run(
        directory.path(),
        &[
            "prepare",
            "--database",
            source.to_str().unwrap(),
            "--operation",
            operation.to_str().unwrap(),
            "--old-executable",
            env!("CARGO_BIN_EXE_engram"),
        ],
    ));
    assert_eq!(fs::read(&source).expect("source retained"), before);
    assert!(!operation.exists());
    assert!(!directory.path().join("must-not-create-home").exists());
}

#[test]
fn upgrade_cli_preserves_committed_wal_and_closes_rollback_before_new_writes() {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    let operation = directory.path().join("upgrade");
    create_wal_source(&source);
    let prepared = success(&prepare(directory.path(), &source, &operation));
    assert_eq!(prepared["phase"], "prepared");
    assert_eq!(prepared["rollback_closed"], false);
    assert_eq!(
        prepared["operational_precondition"],
        "operator attested coordinated downtime; not a proven lock"
    );
    let candidate_bytes = fs::read(operation.join("candidate.db")).expect("candidate bytes");
    // Inspect the detached artifacts during downtime. Even a SQLite read of a
    // WAL-mode source can change its sidecar files, invalidating prepared identity.
    let prepared_source: Vec<_> = ["", "-wal", "-journal"]
        .into_iter()
        .map(|suffix| (suffix, fs::read(sidecar(&source, suffix)).ok()))
        .collect();
    assert_wal_work(&operation.join("backup.db"));
    assert_wal_work(&operation.join("candidate.db"));
    let expected_objects = objects(&operation.join("backup.db"));
    assert_eq!(objects(&operation.join("candidate.db")), expected_objects);
    assert_eq!(
        fs::read(operation.join("old-executable")).unwrap(),
        fs::read(env!("CARGO_BIN_EXE_engram")).unwrap()
    );
    for (suffix, before) in prepared_source {
        assert_eq!(
            fs::read(sidecar(&source, suffix)).ok(),
            before,
            "prepared source component {suffix:?} changed during artifact inspection"
        );
    }

    let activated = success(&step(directory.path(), &operation, "activate"));
    assert_eq!(activated["phase"], "activated");
    assert_eq!(activated["rollback_closed"], false);
    assert!(operation.join("retained-original.db").exists());
    assert_eq!(objects(&source), expected_objects);
    assert_wal_work(&source);
    let before_verification = directory_snapshot(&operation);
    let live_before_verification = fs::read(&source).expect("activated bytes");
    assert_eq!(
        success(&run(
            directory.path(),
            &["status", "--operation", operation.to_str().unwrap()]
        ))["phase"],
        "activated"
    );
    // Ordinary strict opening changes journal mode. Do extra doctor checks on
    // a disposable detached copy, never on the pre-finalize live target.
    let verification = directory.path().join("detached-verification.db");
    fs::copy(&source, &verification).expect("detached verification copy");
    let checked =
        engram::SqliteStore::open_unresolved(&verification).expect("strict detached open");
    assert!(checked.verify_all().expect("detached doctor").is_healthy());
    drop(checked);
    assert_eq!(
        fs::read(&source).expect("unchanged live"),
        live_before_verification
    );
    assert_eq!(directory_snapshot(&operation), before_verification);
    let finalized = success(&step(directory.path(), &operation, "finalize"));
    assert_eq!(finalized["phase"], "finalized");
    assert_eq!(finalized["rollback_closed"], true);
    assert_eq!(
        success(&step(directory.path(), &operation, "finalize"))["rollback_closed"],
        true
    );

    add_root(&source, "New work after resume", "after-resume");
    let after_write = objects(&source);
    assert_ne!(after_write, expected_objects);
    assert_eq!(
        fs::read(operation.join("candidate.db")).expect("immutable candidate"),
        candidate_bytes
    );
    refusal(&step(directory.path(), &operation, "rollback"));
    assert_eq!(objects(&source), after_write);
    let recovered = success(&step(directory.path(), &operation, "recover"));
    assert_eq!(recovered["phase"], "finalized");
    assert_eq!(recovered["rollback_closed"], true);
    assert_eq!(objects(&source), after_write);
    let store = engram::SqliteStore::open_unresolved(&source).expect("strict activated store");
    assert!(store.verify_all().expect("doctor").is_healthy());
    assert!(!directory.path().join("must-not-create-home").exists());
}

#[test]
fn upgrade_cli_rollback_before_finalize_restores_original_wal_content() {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    let operation = directory.path().join("upgrade");
    create_wal_source(&source);
    success(&prepare(directory.path(), &source, &operation));
    let expected = objects(&operation.join("backup.db"));
    success(&step(directory.path(), &operation, "activate"));
    let rolled_back = success(&step(directory.path(), &operation, "rollback"));
    assert_eq!(rolled_back["phase"], "rolled_back");
    assert_eq!(objects(&source), expected);
    assert_wal_work(&source);
    assert!(operation.join("backup.db").exists());
    assert!(operation.join("archive.db").exists());
    refusal(&step(directory.path(), &operation, "finalize"));
    assert_eq!(objects(&source), expected);
}

#[test]
fn upgrade_cli_refuses_source_changes_since_prepare_without_discarding_them() {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    let operation = directory.path().join("upgrade");
    add_root(&source, "Original", "original");
    success(&prepare(directory.path(), &source, &operation));
    add_root(&source, "Unexpected external write", "external-write");
    let changed = objects(&source);
    refusal(&step(directory.path(), &operation, "activate"));
    assert_eq!(objects(&source), changed);
    assert!(!operation.join("retained-original.db").exists());
}

#[test]
fn upgrade_cli_refuses_rollback_when_activated_store_received_unknown_writes() {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    let operation = directory.path().join("upgrade");
    add_root(&source, "Original", "original");
    success(&prepare(directory.path(), &source, &operation));
    success(&step(directory.path(), &operation, "activate"));
    add_root(&source, "Premature external write", "premature-write");
    let changed = objects(&source);
    refusal(&step(directory.path(), &operation, "rollback"));
    assert_eq!(objects(&source), changed);
    assert!(operation.join("retained-original.db").exists());
}

#[test]
fn upgrade_cli_damaged_finalize_record_never_reopens_rollback() {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    let operation = directory.path().join("upgrade");
    add_root(&source, "Original", "original");
    success(&prepare(directory.path(), &source, &operation));
    success(&step(directory.path(), &operation, "activate"));
    success(&step(directory.path(), &operation, "finalize"));
    let before = objects(&source);
    let mut journal: Vec<_> = fs::read_dir(operation.join("journal"))
        .expect("journal directory")
        .map(|entry| entry.expect("journal entry").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    journal.sort();
    let final_record = journal.last().expect("final record");
    let record = fs::read(final_record).expect("record bytes");
    assert!(!record.is_empty());
    fs::write(final_record, &record[..record.len() / 2]).expect("simulate torn record");
    refusal(&step(directory.path(), &operation, "rollback"));
    refusal(&step(directory.path(), &operation, "recover"));
    refusal(&run(
        directory.path(),
        &["status", "--operation", operation.to_str().unwrap()],
    ));
    assert_eq!(objects(&source), before);
    assert!(operation.join("retained-original.db").exists());
}

#[test]
fn upgrade_cli_changed_candidate_refuses_activation_and_preserves_source() {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    let operation = directory.path().join("upgrade");
    add_root(&source, "Original", "original");
    success(&prepare(directory.path(), &source, &operation));
    let before = objects(&source);
    let backup = fs::read(operation.join("backup.db")).expect("backup");
    fs::write(
        operation.join("candidate.db"),
        b"not the verified candidate",
    )
    .expect("damage private fixture candidate");
    refusal(&step(directory.path(), &operation, "activate"));
    assert_eq!(objects(&source), before);
    assert_eq!(
        fs::read(operation.join("backup.db")).expect("retained backup"),
        backup
    );
    assert!(!operation.join("retained-original.db").exists());
}

#[test]
fn upgrade_cli_refuses_unsupported_profile_without_replacing_source() {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    let operation = directory.path().join("upgrade");
    let connection = rusqlite::Connection::open(&source).expect("source");
    connection
        .execute_batch("CREATE TABLE unrelated(value); INSERT INTO unrelated VALUES ('keep me');")
        .expect("unknown layout");
    drop(connection);
    let before = fs::read(&source).expect("before");
    refusal(&prepare(directory.path(), &source, &operation));
    assert_eq!(fs::read(&source).expect("source retained"), before);
    assert!(!operation.join("candidate.db").exists());
    assert!(!operation.join("retained-original.db").exists());
}

#[test]
fn upgrade_cli_status_is_read_only_and_mutating_steps_require_attestation() {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    let operation = directory.path().join("upgrade");
    add_root(&source, "Original", "original");
    success(&prepare(directory.path(), &source, &operation));
    let before = fs::read(&source).expect("source bytes");
    let status = success(&run(
        directory.path(),
        &["status", "--operation", operation.to_str().unwrap()],
    ));
    assert_eq!(status["phase"], "prepared");
    for action in ["activate", "rollback", "finalize", "recover"] {
        refusal(&run(
            directory.path(),
            &[action, "--operation", operation.to_str().unwrap()],
        ));
        assert_eq!(fs::read(&source).expect("unchanged source"), before);
        assert!(!operation.join("retained-original.db").exists());
    }
    assert_eq!(
        fs::read_dir(operation.join("journal"))
            .expect("journal")
            .count(),
        1
    );
    assert!(!directory.path().join("must-not-create-home").exists());
}

#[test]
fn upgrade_cli_relative_prepare_paths_remain_bound_after_changing_directory() {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    let operation = directory.path().join("upgrade");
    let elsewhere = directory.path().join("elsewhere");
    fs::create_dir(&elsewhere).expect("second working directory");
    add_root(&source, "Actual source", "actual");
    let decoy = elsewhere.join("source.db");
    add_root(&decoy, "Unrelated store", "decoy");
    let decoy_before = fs::read(&decoy).expect("decoy bytes");
    success(&prepare(
        directory.path(),
        Path::new("source.db"),
        Path::new("upgrade"),
    ));
    let expected = objects(&operation.join("candidate.db"));
    let status = success(&run(
        &elsewhere,
        &["status", "--operation", operation.to_str().unwrap()],
    ));
    let bound_database = Path::new(status["database"].as_str().expect("database path"));
    assert!(bound_database.is_absolute());
    assert_eq!(
        fs::canonicalize(bound_database).expect("bound database"),
        fs::canonicalize(&source).expect("actual database")
    );
    assert_eq!(
        success(&step(&elsewhere, &operation, "activate"))["phase"],
        "activated"
    );
    assert_eq!(objects(&source), expected);
    assert_eq!(fs::read(&decoy).expect("decoy untouched"), decoy_before);
}

#[test]
fn upgrade_cli_recover_rollback_intent_refuses_unknown_live_writes() {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    let operation = directory.path().join("upgrade");
    add_root(&source, "Original", "original");
    success(&prepare(directory.path(), &source, &operation));
    success(&step(directory.path(), &operation, "activate"));

    // Model a process interruption after publishing rollback intent but before
    // any filesystem effect. The next process must still inspect actual files.
    append_rollback_intent(&operation);

    add_root(&source, "Unknown write after interruption", "unknown-write");
    let changed = objects(&source);
    let retained = fs::read(operation.join("retained-original.db")).expect("retained");
    refusal(&step(directory.path(), &operation, "recover"));
    assert_eq!(objects(&source), changed);
    assert_eq!(
        fs::read(operation.join("retained-original.db")).expect("original retained"),
        retained
    );
}

#[test]
fn upgrade_cli_finalize_never_reports_success_with_an_illegal_journal_transition() {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    let operation = directory.path().join("upgrade");
    add_root(&source, "Original", "original");
    success(&prepare(directory.path(), &source, &operation));
    success(&step(directory.path(), &operation, "activate"));

    // Recreate the state after live-file publication but before its completion
    // record. Actual process-exit injection is covered by the storage tests.
    let mut records: Vec<_> = fs::read_dir(operation.join("journal"))
        .expect("journal")
        .map(|entry| entry.expect("entry").path())
        .collect();
    records.sort();
    let last = records.last().expect("last record");
    let value: Value = serde_json::from_slice(&fs::read(last).expect("record")).expect("JSON");
    assert_eq!(value["kind"], "activated");
    fs::remove_file(last).expect("simulate missing completion record");

    assert_eq!(
        success(&step(directory.path(), &operation, "finalize"))["phase"],
        "finalized"
    );
    let mut records: Vec<_> = fs::read_dir(operation.join("journal"))
        .expect("journal")
        .map(|entry| entry.expect("entry").path())
        .collect();
    records.sort();
    let kinds: Vec<String> = records
        .iter()
        .map(|path| {
            let record: Value =
                serde_json::from_slice(&fs::read(path).expect("record")).expect("JSON");
            record["kind"].as_str().expect("kind").to_owned()
        })
        .collect();
    assert_eq!(kinds, ["prepared", "activating", "activated", "finalized"]);
    assert_eq!(
        success(&run(
            directory.path(),
            &["status", "--operation", operation.to_str().unwrap()],
        ))["phase"],
        "finalized"
    );
    refusal(&step(directory.path(), &operation, "rollback"));
}

#[test]
fn upgrade_cli_reserved_sidecar_operation_paths_refuse_before_effects() {
    // T1/C6: conservative admission applies even on case-sensitive test hosts,
    // because the same platform can mount a case-insensitive filesystem.
    let suffixes = ["-wal", "-shm", "-journal", "-WAL", "-SHM", "-JOURNAL"];
    for suffix in suffixes {
        for descendant in [false, true] {
            let directory = test_support::temp_home().expect("fixture");
            let source = directory.path().join("source.db");
            add_root(&source, "Original", "original");
            let reserved = sidecar(&source, suffix);
            assert!(
                !reserved.exists(),
                "fixture must test an absent reserved path"
            );
            let operation = if descendant {
                reserved.join("operation")
            } else {
                reserved
            };
            let before = directory_snapshot(directory.path());
            refusal(&prepare(directory.path(), &source, &operation));
            assert_eq!(
                directory_snapshot(directory.path()),
                before,
                "reserved suffix {suffix}, descendant {descendant}"
            );
        }
    }
}

#[test]
fn upgrade_cli_ambiguous_unicode_sidecar_paths_refuse_before_effects() {
    // T1/C6: admission must be conservative independently of the test mount's
    // case/normalization rules. The Windows test below checks a real alias.
    for alias in ["É.db-wal", "e\u{301}.db-wal"] {
        for descendant in [false, true] {
            let directory = test_support::temp_home().expect("fixture");
            let source = directory.path().join("é.db");
            add_root(&source, "Original", "original");
            let reserved = directory.path().join(alias);
            assert!(!reserved.exists(), "reserved sidecar must be absent");
            let operation = if descendant {
                reserved.join("operation")
            } else {
                reserved
            };
            let before = directory_snapshot(directory.path());
            refusal(&prepare(directory.path(), &source, &operation));
            assert_eq!(directory_snapshot(directory.path()), before);
        }
    }
}

#[cfg(windows)]
#[test]
fn upgrade_cli_unicode_case_sidecar_aliases_refuse_before_effects() {
    for suffix in ["-wal", "-shm", "-journal"] {
        for descendant in [false, true] {
            let directory = test_support::temp_home().expect("fixture");
            let source = directory.path().join("é.db");
            add_root(&source, "Original", "original");
            let alias = directory.path().join("É.db");
            assert_eq!(
                fs::canonicalize(&alias).expect("Windows case alias"),
                fs::canonicalize(&source).expect("source"),
                "fixture must exercise a real filesystem case alias"
            );
            let reserved = sidecar(&alias, suffix);
            assert!(!reserved.exists(), "reserved sidecar must be absent");
            let operation = if descendant {
                reserved.join("operation")
            } else {
                reserved
            };
            let before = directory_snapshot(directory.path());
            refusal(&prepare(directory.path(), &source, &operation));
            assert_eq!(
                directory_snapshot(directory.path()),
                before,
                "Unicode alias {suffix}, descendant {descendant} changed fixture"
            );
        }
    }
}

#[test]
fn upgrade_cli_rollback_intent_cannot_activate_or_finalize_before_recovery() {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    let operation = directory.path().join("upgrade");
    add_root(&source, "Original", "original");
    success(&prepare(directory.path(), &source, &operation));
    success(&step(directory.path(), &operation, "activate"));
    append_rollback_intent(&operation);
    let before = directory_snapshot(directory.path());
    assert_eq!(
        success(&run(
            directory.path(),
            &["status", "--operation", operation.to_str().unwrap()]
        ))["phase"],
        "rolling_back"
    );
    for action in ["activate", "finalize"] {
        refusal(&step(directory.path(), &operation, action));
        assert_eq!(
            directory_snapshot(directory.path()),
            before,
            "refused {action} changed files"
        );
    }
    assert_eq!(
        success(&step(directory.path(), &operation, "recover"))["phase"],
        "rolled_back"
    );
}

#[test]
fn upgrade_cli_rolled_back_remains_terminal_after_old_consumers_resume() {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    let operation = directory.path().join("upgrade");
    add_root(&source, "Original", "original");
    success(&prepare(directory.path(), &source, &operation));
    success(&step(directory.path(), &operation, "activate"));
    assert_eq!(
        success(&step(directory.path(), &operation, "rollback"))["phase"],
        "rolled_back"
    );
    add_root(
        &source,
        "Legitimate old-version work after rollback",
        "after-rollback",
    );
    let before = directory_snapshot(directory.path());
    assert_eq!(
        success(&run(
            directory.path(),
            &["status", "--operation", operation.to_str().unwrap()]
        ))["phase"],
        "rolled_back"
    );
    assert_eq!(
        success(&step(directory.path(), &operation, "recover"))["phase"],
        "rolled_back"
    );
    refusal(&step(directory.path(), &operation, "activate"));
    refusal(&step(directory.path(), &operation, "finalize"));
    assert_eq!(directory_snapshot(directory.path()), before);
}

#[test]
fn upgrade_cli_premature_strict_open_refuses_without_discarding_any_files() {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    let operation = directory.path().join("upgrade");
    add_root(&source, "Original", "original");
    success(&prepare(directory.path(), &source, &operation));
    success(&step(directory.path(), &operation, "activate"));
    let published = fs::read(&source).expect("published candidate");
    let premature = engram::SqliteStore::open_unresolved(&source).expect("premature ordinary open");
    assert!(
        premature
            .verify_all()
            .expect("premature doctor")
            .is_healthy()
    );
    drop(premature);
    assert_ne!(
        fs::read(&source).expect("mode-changed live"),
        published,
        "must exercise an actual mode change"
    );
    let before = directory_snapshot(directory.path());
    for action in ["finalize", "rollback", "recover"] {
        refusal(&step(directory.path(), &operation, action));
        assert_eq!(
            directory_snapshot(directory.path()),
            before,
            "refused {action} changed files"
        );
    }
    assert!(operation.join("retained-original.db").exists());
    assert!(operation.join("candidate.db").exists());
}

#[test]
fn upgrade_cli_double_interruption_recovers_split_original_components() {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    let operation = directory.path().join("upgrade");
    create_wal_source(&source);
    let original: Vec<_> = ["", "-wal", "-journal"]
        .into_iter()
        .map(|suffix| (suffix, fs::read(sidecar(&source, suffix)).ok()))
        .collect();
    success(&prepare(directory.path(), &source, &operation));
    append_journal_transition(&operation, "prepared", "activating");
    // Model interruption after retaining WAL but before moving original main,
    // followed by interruption after recording rollback intent. Actual child
    // exit injection belongs to the storage tests; this checks a new CLI process.
    fs::rename(
        sidecar(&source, "-wal"),
        sidecar(&operation.join("retained-original.db"), "-wal"),
    )
    .expect("model WAL retention");
    append_journal_transition(&operation, "activating", "rolling_back");
    assert_eq!(
        success(&step(directory.path(), &operation, "recover"))["phase"],
        "rolled_back"
    );
    for (suffix, bytes) in original {
        assert_eq!(
            fs::read(sidecar(&source, suffix)).ok(),
            bytes,
            "restored component {suffix}"
        );
    }
    assert_wal_work(&source);
}

#[test]
fn upgrade_cli_contradictory_retained_original_refuses_before_effects() {
    for remove_retained in [false, true] {
        let directory = test_support::temp_home().expect("fixture");
        let source = directory.path().join("source.db");
        let operation = directory.path().join("upgrade");
        add_root(&source, "Original", "original");
        success(&prepare(directory.path(), &source, &operation));
        success(&step(directory.path(), &operation, "activate"));
        let mut records: Vec<_> = fs::read_dir(operation.join("journal"))
            .expect("journal")
            .map(|entry| entry.expect("entry").path())
            .collect();
        records.sort();
        let last = records.last().expect("completion record");
        let completed: Value =
            serde_json::from_slice(&fs::read(last).expect("record")).expect("JSON");
        assert_eq!(completed["kind"], "activated");
        fs::remove_file(last).expect("model interrupted activation completion");
        let retained = operation.join("retained-original.db");
        if remove_retained {
            fs::remove_file(&retained).expect("model missing retained original");
        } else {
            fs::write(&retained, b"contradictory retained bytes")
                .expect("model damaged retained original");
        }
        let before = directory_snapshot(directory.path());
        for action in ["rollback", "recover", "activate", "finalize"] {
            refusal(&step(directory.path(), &operation, action));
            assert_eq!(
                directory_snapshot(directory.path()),
                before,
                "refused {action}, missing retained {remove_retained}"
            );
        }
    }
}

#[test]
fn upgrade_cli_rollback_refuses_new_durable_sidecars_before_effects() {
    for suffix in ["-wal", "-journal"] {
        let directory = test_support::temp_home().expect("fixture");
        let source = directory.path().join("source.db");
        let operation = directory.path().join("upgrade");
        add_root(&source, "Original", "original");
        assert!(!sidecar(&source, suffix).exists());
        success(&prepare(directory.path(), &source, &operation));
        success(&step(directory.path(), &operation, "activate"));
        append_rollback_intent(&operation);
        fs::write(sidecar(&source, suffix), b"unaccounted durable sidecar")
            .expect("model unknown sidecar writes during rollback");
        let before = directory_snapshot(directory.path());
        for action in ["rollback", "recover"] {
            refusal(&step(directory.path(), &operation, action));
            assert_eq!(
                directory_snapshot(directory.path()),
                before,
                "refused {action} changed files with unknown {suffix}"
            );
        }
    }
}

#[test]
fn upgrade_cli_preexisting_partial_publication_refuses_before_retain() {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    let operation = directory.path().join("upgrade");
    add_root(&source, "Original", "original");
    success(&prepare(directory.path(), &source, &operation));
    fs::write(operation.join("publish-staging.partial"), b"unowned bytes")
        .expect("preexisting partial");
    let before = directory_snapshot(directory.path());
    refusal(&step(directory.path(), &operation, "activate"));
    assert_eq!(directory_snapshot(directory.path()), before);
}

#[test]
fn upgrade_cli_partial_publication_recovers_only_with_accounted_original() {
    for action in ["activate", "recover", "rollback"] {
        for corruption in [None, Some("retained"), Some("partial"), Some("sidecar")] {
            let directory = test_support::temp_home().expect("fixture");
            let source = directory.path().join("source.db");
            let operation = directory.path().join("upgrade");
            add_root(&source, "Original", "original");
            let original = fs::read(&source).expect("original bytes");
            success(&prepare(directory.path(), &source, &operation));
            let candidate = fs::read(operation.join("candidate.db")).expect("candidate bytes");
            assert!(candidate.len() > 8192);
            for suffix in ["-wal", "-shm", "-journal"] {
                assert!(!sidecar(&source, suffix).exists());
            }
            // Reconstruct a process stopped inside the candidate copy, after
            // all original components were retained. Unit child-exit tests
            // separately exercise the real copy interruption hook.
            append_journal_transition(&operation, "prepared", "activating");
            let retained = operation.join("retained-original.db");
            fs::rename(&source, &retained).expect("model retained main");
            let partial = operation.join("publish-staging.partial");
            fs::write(&partial, &candidate[..8192]).expect("model partial candidate copy");
            if let Some(corruption) = corruption {
                let changed = match corruption {
                    "retained" => retained.clone(),
                    "partial" => partial.clone(),
                    "sidecar" => sidecar(&source, "-wal"),
                    _ => unreachable!("fixture corruption kind"),
                };
                fs::write(changed, b"unaccounted bytes").expect("model unknown writes");
                let before = directory_snapshot(directory.path());
                refusal(&step(directory.path(), &operation, action));
                assert_eq!(
                    directory_snapshot(directory.path()),
                    before,
                    "{action} must refuse before deleting any partial or original ({corruption})"
                );
            } else {
                let report = success(&step(directory.path(), &operation, action));
                let rollback = action == "rollback";
                assert_eq!(
                    report["phase"],
                    if rollback { "rolled_back" } else { "activated" }
                );
                assert_eq!(
                    fs::read(&source).expect("restored live path"),
                    if rollback { original } else { candidate }
                );
                assert!(
                    !partial.exists(),
                    "completed operation leaves no partial copy"
                );
                assert!(!operation.join("publish-staging").exists());
            }
        }
    }
}

#[test]
fn upgrade_cli_journal_partial_recovers_only_expected_record_bytes() {
    for corruption in [None, Some("temp"), Some("candidate")] {
        let directory = test_support::temp_home().expect("fixture");
        let source = directory.path().join("source.db");
        let operation = directory.path().join("upgrade");
        add_root(&source, "Original", "original");
        success(&prepare(directory.path(), &source, &operation));
        success(&step(directory.path(), &operation, "activate"));
        let mut paths: Vec<_> = fs::read_dir(operation.join("journal"))
            .expect("journal")
            .map(|entry| entry.expect("entry").path())
            .collect();
        paths.sort();
        let last = paths.last().expect("activation completion record");
        let bytes = fs::read(last).expect("actual serialized record");
        let record: Value = serde_json::from_slice(&bytes).expect("record JSON");
        assert_eq!(record["kind"], "activated");
        // Reconstruct interruption during serialization using an exact prefix
        // of a real emitted record, independent of private serializer helpers.
        fs::remove_file(last).expect("model unpublished completion record");
        let temp = operation.join("journal-publish.tmp");
        fs::write(
            &temp,
            if corruption == Some("temp") {
                b"unaccounted record bytes"
            } else {
                &bytes[..bytes.len() / 2]
            },
        )
        .expect("model interrupted record write");
        if corruption == Some("candidate") {
            fs::write(
                operation.join("candidate.db"),
                b"changed candidate artifact",
            )
            .expect("model artifact corruption");
        }
        if corruption.is_some() {
            let before = directory_snapshot(directory.path());
            refusal(&step(directory.path(), &operation, "finalize"));
            assert_eq!(directory_snapshot(directory.path()), before);
        } else {
            assert_eq!(
                success(&step(directory.path(), &operation, "finalize"))["phase"],
                "finalized"
            );
            assert!(!temp.exists());
            assert_eq!(fs::read(last).expect("completed activation record"), bytes);
            let before = directory_snapshot(directory.path());
            refusal(&step(directory.path(), &operation, "rollback"));
            assert_eq!(directory_snapshot(directory.path()), before);
        }
    }
}

// State contract T2/C1: contradiction is visible before intent or sidecar moves.
#[test]
fn upgrade_cli_model_prepared_wrong_retained_preserves_inventory() {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    let operation = directory.path().join("upgrade");
    create_wal_source(&source);
    success(&prepare(directory.path(), &source, &operation));
    fs::write(
        operation.join("retained-original.db"),
        b"foreign retained main",
    )
    .expect("contradictory destination");
    let before = directory_snapshot(directory.path());
    refusal(&step(directory.path(), &operation, "activate"));
    assert_eq!(directory_snapshot(directory.path()), before);
}

// T4/C1: the intact Activated record is not proof that rollback material exists.
#[test]
fn upgrade_cli_model_activated_missing_retained_refuses_status_and_recover() {
    for action in ["status", "recover"] {
        let directory = test_support::temp_home().expect("fixture");
        let source = directory.path().join("source.db");
        let operation = directory.path().join("upgrade");
        add_root(&source, "Original", "original");
        success(&prepare(directory.path(), &source, &operation));
        assert_eq!(
            success(&step(directory.path(), &operation, "activate"))["phase"],
            "activated"
        );
        fs::remove_file(operation.join("retained-original.db")).expect("missing retained original");
        let before = directory_snapshot(directory.path());
        refusal(&step(directory.path(), &operation, action));
        assert_eq!(directory_snapshot(directory.path()), before, "{action}");
    }
}

// T4/T7/C2: no owned-temp cleanup may hide a contradiction in another reserved name.
#[test]
fn upgrade_cli_model_combined_temps_refuse_before_cleanup() {
    for foreign_name in ["publish-staging", "publish-staging.partial"] {
        for action in ["finalize", "recover", "activate"] {
            let directory = test_support::temp_home().expect("fixture");
            let source = directory.path().join("source.db");
            let operation = directory.path().join("upgrade");
            add_root(&source, "Original", "original");
            success(&prepare(directory.path(), &source, &operation));
            success(&step(directory.path(), &operation, "activate"));
            let record = operation.join("journal").join("00000003.json");
            let bytes = fs::read(&record).expect("activated record");
            assert_eq!(
                serde_json::from_slice::<Value>(&bytes).expect("JSON")["kind"],
                "activated"
            );
            fs::remove_file(record).expect("model unpublished completion");
            fs::write(
                operation.join("journal-publish.tmp"),
                &bytes[..bytes.len() / 2],
            )
            .expect("legal unpublished prefix");
            fs::write(operation.join(foreign_name), b"foreign publication bytes")
                .expect("contradictory reserved name");
            let before = directory_snapshot(directory.path());
            refusal(&step(directory.path(), &operation, action));
            assert_eq!(
                directory_snapshot(directory.path()),
                before,
                "{action}: {foreign_name}"
            );
        }
    }
}

// T9/T10/C2: no-op/intent-only branches still admit the whole cleanup plan.
#[test]
fn upgrade_cli_prepared_combined_temps_refuse_before_cleanup() {
    for intent in [false, true] {
        for action in ["recover", "rollback"] {
            let directory = test_support::temp_home().expect("fixture");
            let source = directory.path().join("source.db");
            let operation = directory.path().join("upgrade");
            add_root(&source, "Original", "original");
            success(&prepare(directory.path(), &source, &operation));
            if intent {
                append_journal_transition(&operation, "prepared", "activating");
            }
            let (previous, next, sequence) = if intent {
                ("activating", "rolling_back", 3)
            } else {
                ("prepared", "activating", 2)
            };
            append_journal_transition(&operation, previous, next);
            let path = operation
                .join("journal")
                .join(format!("{sequence:08}.json"));
            let bytes = fs::read(&path).expect("legal next record");
            fs::remove_file(path).expect("model unpublished next record");
            fs::write(
                operation.join("journal-publish.tmp"),
                &bytes[..bytes.len() / 2],
            )
            .expect("legal journal prefix");
            fs::write(operation.join("publish-staging"), b"foreign staging")
                .expect("contradiction already present");
            let before = directory_snapshot(directory.path());
            refusal(&step(directory.path(), &operation, action));
            assert_eq!(directory_snapshot(directory.path()), before);
        }
    }
}

// T5/C3: a complete copy before the staging link is still cleanup owed by rollback.
#[test]
fn upgrade_cli_model_complete_partial_rollback_cleans_up() {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    let operation = directory.path().join("upgrade");
    add_root(&source, "Original", "original");
    let original = fs::read(&source).expect("original");
    success(&prepare(directory.path(), &source, &operation));
    append_journal_transition(&operation, "prepared", "activating");
    fs::rename(&source, operation.join("retained-original.db")).expect("retained main");
    let partial = operation.join("publish-staging.partial");
    fs::copy(operation.join("candidate.db"), &partial).expect("complete unpublished copy");
    assert_eq!(
        success(&step(directory.path(), &operation, "rollback"))["phase"],
        "rolled_back"
    );
    assert_eq!(fs::read(&source).expect("restored main"), original);
    assert!(
        !partial.exists(),
        "terminal rollback must clean the accounted complete partial"
    );
}

// C9: a manually restored original cannot erase an Activated journal decision.
#[test]
fn upgrade_cli_model_hand_restored_activated_state_is_not_prepared() {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    let operation = directory.path().join("upgrade");
    add_root(&source, "Original", "original");
    success(&prepare(directory.path(), &source, &operation));
    success(&step(directory.path(), &operation, "activate"));
    fs::remove_file(&source).expect("simulate manual removal of candidate");
    fs::rename(operation.join("retained-original.db"), &source).expect("simulate hand restoration");
    let before = directory_snapshot(directory.path());
    for action in ["status", "rollback", "recover", "activate"] {
        refusal(&step(directory.path(), &operation, action));
        assert_eq!(directory_snapshot(directory.path()), before, "{action}");
    }
}

// T9/T10 positive controls: valid intent-only recovery is not a contradiction.
#[test]
fn upgrade_cli_prepared_rollback_distinguishes_intent() {
    for intent in [false, true] {
        let directory = test_support::temp_home().expect("fixture");
        let source = directory.path().join("source.db");
        let operation = directory.path().join("upgrade");
        add_root(&source, "Original", "original");
        success(&prepare(directory.path(), &source, &operation));
        let original = fs::read(&source).expect("original main");
        if intent {
            append_journal_transition(&operation, "prepared", "activating");
        }
        let before = directory_snapshot(directory.path());
        let phase = if intent { "rolled_back" } else { "prepared" };
        assert_eq!(
            success(&step(directory.path(), &operation, "rollback"))["phase"],
            phase
        );
        assert_eq!(fs::read(&source).expect("unchanged main"), original);
        if intent {
            let mut records: Vec<_> = fs::read_dir(operation.join("journal"))
                .expect("journal")
                .map(|entry| entry.expect("entry").path())
                .collect();
            records.sort();
            let kinds: Vec<_> = records
                .into_iter()
                .map(|path| {
                    let record: Value =
                        serde_json::from_slice(&fs::read(path).expect("record")).expect("JSON");
                    record["kind"].as_str().expect("kind").to_owned()
                })
                .collect();
            assert_eq!(
                kinds,
                ["prepared", "activating", "rolling_back", "rolled_back"]
            );
            let terminal = directory_snapshot(directory.path());
            refusal(&step(directory.path(), &operation, "activate"));
            assert_eq!(directory_snapshot(directory.path()), terminal);
        } else {
            assert_eq!(
                directory_snapshot(directory.path()),
                before,
                "Prepared rollback is a no-op"
            );
        }
    }
}

// C9: legal journal records do not authorize a contradictory file placement.
// Build a real activation, then remove records/files to model external damage;
// no expected result is derived from the production phase classifier.
fn assert_journal_file_contradiction_refuses(prepared_only: bool, live_missing: bool) {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    let operation = directory.path().join("upgrade");
    add_root(&source, "Original", "original");
    success(&prepare(directory.path(), &source, &operation));
    success(&step(directory.path(), &operation, "activate"));
    let mut records: Vec<_> = fs::read_dir(operation.join("journal"))
        .expect("journal")
        .map(|entry| entry.expect("entry").path())
        .collect();
    records.sort();
    let kinds: Vec<_> = records
        .iter()
        .map(|path| {
            let record: Value =
                serde_json::from_slice(&fs::read(path).expect("record")).expect("JSON");
            record["kind"].as_str().expect("kind").to_owned()
        })
        .collect();
    assert_eq!(kinds, ["prepared", "activating", "activated"]);
    if prepared_only {
        for path in &records[1..] {
            fs::remove_file(path).expect("model lost intent and completion");
        }
    }
    if live_missing {
        fs::remove_file(&source).expect("model missing live database");
    }
    assert!(operation.join("retained-original.db").is_file());
    fs::copy(
        operation.join("candidate.db"),
        operation.join("publish-staging"),
    )
    .expect("candidate staging must not be cleaned in a contradictory state");
    let before = directory_snapshot(directory.path());
    for action in ["activate", "recover", "finalize", "status", "rollback"] {
        refusal(&step(directory.path(), &operation, action));
        assert_eq!(
            directory_snapshot(directory.path()),
            before,
            "{action}: prepared_only={prepared_only}, live_missing={live_missing}"
        );
    }
}

#[test]
fn upgrade_cli_review_v5_activated_missing_live_refuses_before_effects() {
    assert_journal_file_contradiction_refuses(false, true);
}

#[test]
fn upgrade_cli_review_v5_prepared_retained_missing_live_refuses_before_effects() {
    assert_journal_file_contradiction_refuses(true, true);
}

#[test]
fn upgrade_cli_review_v5_prepared_retained_candidate_live_refuses_before_effects() {
    assert_journal_file_contradiction_refuses(true, false);
}

#[test]
fn upgrade_cli_review_v5_status_helper_reads_prepared_without_effects() {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    let operation = directory.path().join("upgrade");
    add_root(&source, "Original", "original");
    success(&prepare(directory.path(), &source, &operation));
    let before = directory_snapshot(directory.path());
    let report = success(&step(directory.path(), &operation, "status"));
    assert_eq!(report["phase"], "prepared");
    assert_eq!(report["rollback_closed"], false);
    assert_eq!(directory_snapshot(directory.path()), before);
}

// C9: intact rollback material does not make a hand-restored original a candidate.
#[test]
fn upgrade_cli_review_v5_activated_original_live_with_retained_refuses_before_effects() {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    let operation = directory.path().join("upgrade");
    add_root(&source, "Original", "original");
    success(&prepare(directory.path(), &source, &operation));
    success(&step(directory.path(), &operation, "activate"));
    let retained = operation.join("retained-original.db");
    let original = fs::read(&retained).expect("retained original");
    let candidate = fs::read(operation.join("candidate.db")).expect("candidate");
    assert_ne!(
        original, candidate,
        "fixture must distinguish the two identities"
    );
    fs::copy(&retained, &source).expect("model hand-restored copy, retained still intact");
    fs::copy(
        operation.join("candidate.db"),
        operation.join("publish-staging"),
    )
    .expect("owned staging must remain on contradiction");
    let before = directory_snapshot(directory.path());
    for action in ["status", "activate", "recover", "finalize", "rollback"] {
        refusal(&step(directory.path(), &operation, action));
        assert_eq!(directory_snapshot(directory.path()), before, "{action}");
    }
}

// C7/C9: Activating intent cannot make a candidate recoverable when neither
// original main location remains. Status must not bless this contradiction.
fn missing_original_refuses_without_effects(candidate_live: bool, retained_shm: bool) {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    let operation = directory.path().join("upgrade");
    add_root(&source, "Original", "original");
    success(&prepare(directory.path(), &source, &operation));
    let original = fs::read(&source).expect("original main");
    let candidate = fs::read(operation.join("candidate.db")).expect("candidate");
    assert_ne!(original, candidate, "independent identity discriminator");
    append_journal_transition(&operation, "prepared", "activating");
    if candidate_live {
        fs::copy(operation.join("candidate.db"), &source).expect("model lost original");
        assert_eq!(fs::read(&source).expect("live candidate"), candidate);
    } else {
        fs::remove_file(&source).expect("model missing original");
        assert!(!source.exists());
    }
    assert!(!operation.join("retained-original.db").exists());
    for suffix in ["-wal", "-journal", "-shm"] {
        assert!(!sidecar(&source, suffix).exists());
        assert!(!sidecar(&operation.join("retained-original.db"), suffix).exists());
    }
    if retained_shm {
        fs::write(
            sidecar(&operation.join("retained-original.db"), "-shm"),
            b"retained coordination state cannot replace original main",
        )
        .expect("retained SHM");
    }
    let before = directory_snapshot(directory.path());
    for action in ["status", "activate", "recover", "finalize", "rollback"] {
        refusal(&step(directory.path(), &operation, action));
        assert_eq!(directory_snapshot(directory.path()), before, "{action}");
    }
}

#[test]
fn upgrade_cli_review_v6_candidate_without_original_refuses_without_effects() {
    missing_original_refuses_without_effects(true, false);
}

#[test]
fn upgrade_cli_review_v6_retained_shm_does_not_replace_missing_original() {
    missing_original_refuses_without_effects(true, true);
}

#[test]
fn upgrade_cli_review_v6_both_main_locations_missing_refuses_without_effects() {
    missing_original_refuses_without_effects(false, false);
}

#[test]
fn upgrade_cli_review_v6_retained_shm_without_either_main_refuses_without_effects() {
    missing_original_refuses_without_effects(false, true);
}

// T6/C1/C9: status validates rollback material, not just the intact intent.
// Separate fixtures ensure neither missing-main variant hides the other.
fn rolling_back_missing_original_status_refuses(candidate_live: bool) {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    let operation = directory.path().join("upgrade");
    add_root(&source, "Original", "original");
    success(&prepare(directory.path(), &source, &operation));
    let original = fs::read(&source).expect("original");
    let candidate = fs::read(operation.join("candidate.db")).expect("candidate");
    assert_ne!(original, candidate, "independent identity discriminator");
    success(&step(directory.path(), &operation, "activate"));
    append_journal_transition(&operation, "activated", "rolling_back");
    fs::remove_file(operation.join("retained-original.db")).expect("lost original");
    if candidate_live {
        assert_eq!(fs::read(&source).expect("live candidate"), candidate);
    } else {
        fs::remove_file(&source).expect("lost live main");
    }
    for suffix in ["-wal", "-journal", "-shm"] {
        assert!(!sidecar(&source, suffix).exists());
        assert!(!sidecar(&operation.join("retained-original.db"), suffix).exists());
    }
    let before = directory_snapshot(directory.path());
    refusal(&step(directory.path(), &operation, "status"));
    assert_eq!(directory_snapshot(directory.path()), before);
}

#[test]
fn upgrade_cli_review_v7_rolling_back_candidate_without_original_status_refuses() {
    rolling_back_missing_original_status_refuses(true);
}

#[test]
fn upgrade_cli_review_v7_rolling_back_both_mains_missing_status_refuses() {
    rolling_back_missing_original_status_refuses(false);
}

#[test]
fn upgrade_cli_review_v7_rolling_back_corrupt_retained_twin_status_refuses() {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    let operation = directory.path().join("upgrade");
    add_root(&source, "Original", "original");
    success(&prepare(directory.path(), &source, &operation));
    let original = fs::read(&source).expect("original");
    success(&step(directory.path(), &operation, "activate"));
    append_journal_transition(&operation, "activated", "rolling_back");
    let retained = operation.join("retained-original.db");
    fs::copy(&retained, &source).expect("model restored original");
    fs::write(&retained, b"foreign retained twin").expect("model corruption");
    assert_eq!(fs::read(&source).expect("restored main"), original);
    assert_ne!(fs::read(&retained).expect("corrupt twin"), original);
    let before = directory_snapshot(directory.path());
    refusal(&step(directory.path(), &operation, "status"));
    assert_eq!(directory_snapshot(directory.path()), before);
}

// T6 positive control: status is read-only and has no downtime argument.
#[test]
fn upgrade_cli_review_v7_rolling_back_valid_status_preserves_inventory() {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    let operation = directory.path().join("upgrade");
    add_root(&source, "Original", "original");
    success(&prepare(directory.path(), &source, &operation));
    success(&step(directory.path(), &operation, "activate"));
    append_journal_transition(&operation, "activated", "rolling_back");
    let before = directory_snapshot(directory.path());
    let report = success(&step(directory.path(), &operation, "status"));
    assert_eq!(report["phase"], "rolling_back");
    assert_eq!(report["rollback_closed"], false);
    assert_eq!(directory_snapshot(directory.path()), before);
}

// T6/C2: diagnosis must evaluate reserved ownership without performing cleanup.
fn rolling_back_foreign_temp_status_refuses(name: &str) {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    let operation = directory.path().join("upgrade");
    add_root(&source, "Original", "original");
    success(&prepare(directory.path(), &source, &operation));
    success(&step(directory.path(), &operation, "activate"));
    append_journal_transition(&operation, "activated", "rolling_back");
    let candidate = fs::read(operation.join("candidate.db")).expect("candidate");
    let foreign = b"foreign temporary, not candidate or journal bytes";
    assert!(!candidate.starts_with(foreign));
    fs::write(operation.join(name), foreign).expect("foreign reserved entry");
    let before = directory_snapshot(directory.path());
    refusal(&step(directory.path(), &operation, "status"));
    assert_eq!(directory_snapshot(directory.path()), before);
}

#[test]
fn upgrade_cli_review_v7_foreign_staging_status_refuses() {
    rolling_back_foreign_temp_status_refuses("publish-staging");
}

#[test]
fn upgrade_cli_review_v7_foreign_partial_status_refuses() {
    rolling_back_foreign_temp_status_refuses("publish-staging.partial");
}

#[test]
fn upgrade_cli_review_v7_foreign_journal_temp_status_refuses() {
    rolling_back_foreign_temp_status_refuses("journal-publish.tmp");
}
