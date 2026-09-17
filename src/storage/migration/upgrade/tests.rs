use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::SqliteStore;
use crate::test_support::temp_home;

use super::{
    JournalKind, JournalRecord, MigrationError, OPERATIONAL_PRECONDITION, UpgradeFault,
    UpgradeOperationRequest, UpgradePhase, UpgradePrepareRequest, activate_upgrade,
    finalize_upgrade, inject_prepare_source_mutation, inject_upgrade_exit, inject_upgrade_fault,
    legal_transition, prepare_upgrade, recover_upgrade, rollback_upgrade, upgrade_status,
    validate_journal_records,
};

fn confirmed(operation: &std::path::Path) -> UpgradeOperationRequest {
    UpgradeOperationRequest {
        operation: operation.to_path_buf(),
        offline_confirmed: true,
    }
}

fn overlap_or_unix_not_a_directory(error: &MigrationError) -> bool {
    if error.to_string().contains("overlaps") {
        return true;
    }
    match error {
        MigrationError::Io(io) if cfg!(unix) => {
            io.kind() == std::io::ErrorKind::NotADirectory || io.raw_os_error() == Some(20)
        }
        _ => false,
    }
}

fn fixture() -> (
    crate::test_support::TempHome,
    UpgradePrepareRequest,
    std::path::PathBuf,
) {
    let directory = temp_home().expect("directory");
    let database = directory.path().join("store.db");
    drop(SqliteStore::open_unresolved(&database).expect("current store"));
    let executable = directory.path().join("old-bin");
    fs::write(&executable, b"compatible-old-bytes").expect("executable");
    let request = UpgradePrepareRequest {
        database: database.clone(),
        operation: directory.path().join("operation"),
        old_executable: executable,
        offline_confirmed: true,
    };
    (directory, request, database)
}

#[test]
fn upgrade_prepare_refuses_without_offline_confirmation() {
    let (_directory, mut request, _) = fixture();
    request.offline_confirmed = false;
    let error = prepare_upgrade(&request).expect_err("unconfirmed");
    assert!(error.to_string().contains("offline-confirmed"));
}

#[test]
fn upgrade_prepare_activate_finalize_report_omits_exclusion_verified() {
    let (_directory, request, database) = fixture();
    let original = fs::read(&database).expect("original");
    let prepared = prepare_upgrade(&request).expect("prepare");
    assert_eq!(prepared.phase, UpgradePhase::Prepared);
    assert_eq!(prepared.operational_precondition, OPERATIONAL_PRECONDITION);
    assert!(!prepared.rollback_closed);
    assert!(prepared.retained_original.is_none());
    let encoded = serde_json::to_value(&prepared).expect("json");
    assert!(encoded.get("exclusion_verified").is_none());
    assert_eq!(
        encoded["operational_precondition"],
        OPERATIONAL_PRECONDITION
    );
    assert_eq!(
        upgrade_status(&request.operation).expect("status").phase,
        UpgradePhase::Prepared
    );

    let activated = activate_upgrade(&confirmed(&request.operation)).expect("activate");
    assert_eq!(activated.phase, UpgradePhase::Activated);
    assert!(activated.retained_original.is_some());
    assert_ne!(fs::read(&database).expect("candidate"), original);

    let finalized = finalize_upgrade(&confirmed(&request.operation)).expect("finalize");
    assert_eq!(finalized.phase, UpgradePhase::Finalized);
    assert!(finalized.rollback_closed);
    let refused = rollback_upgrade(&confirmed(&request.operation)).expect_err("closed");
    assert!(refused.to_string().contains("closed"));
}

#[test]
fn upgrade_rollback_restores_original_bytes_before_finalize() {
    let (_directory, request, database) = fixture();
    let original = fs::read(&database).expect("original");
    prepare_upgrade(&request).expect("prepare");
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    let rolled = rollback_upgrade(&confirmed(&request.operation)).expect("rollback");
    assert_eq!(rolled.phase, UpgradePhase::RolledBack);
    assert_eq!(fs::read(&database).expect("restored"), original);
}

#[test]
fn upgrade_activate_refuses_when_live_bytes_change_after_prepare() {
    let (_directory, request, database) = fixture();
    prepare_upgrade(&request).expect("prepare");
    let mut bytes = fs::read(&database).expect("live");
    bytes.push(0);
    fs::write(&database, bytes).expect("mutate");
    let error = activate_upgrade(&confirmed(&request.operation)).expect_err("changed");
    assert!(error.to_string().contains("changed"));
}

#[test]
fn upgrade_recover_refuses_incomplete_prepare_and_keeps_backup() {
    let (_directory, request, _) = fixture();
    inject_upgrade_fault(Some(UpgradeFault::BackupPublished));
    prepare_upgrade(&request).expect_err("fault");
    inject_upgrade_fault(None);
    let error = recover_upgrade(&confirmed(&request.operation)).expect_err("incomplete");
    assert!(error.to_string().contains("incomplete"));
    assert!(request.operation.join("backup.db").is_file());
    assert!(!request.operation.join("archive.db").exists());
}

#[test]
fn upgrade_recover_completes_activate_after_retain_without_journal() {
    let (_directory, request, database) = fixture();
    let original = fs::read(&database).expect("original");
    let prepared = prepare_upgrade(&request).expect("prepare");
    inject_upgrade_fault(Some(UpgradeFault::OriginalRetained));
    activate_upgrade(&confirmed(&request.operation)).expect_err("fault");
    inject_upgrade_fault(None);
    let recovered = recover_upgrade(&confirmed(&request.operation)).expect("recover");
    assert_eq!(recovered.phase, UpgradePhase::Activated);
    assert_ne!(fs::read(&database).expect("live"), original);
    assert_eq!(
        serde_json::to_value(&recovered).expect("json")["candidate_sha256"],
        prepared.candidate_sha256
    );
}

#[test]
fn upgrade_recover_records_activated_when_live_already_switched() {
    let (_directory, request, _) = fixture();
    prepare_upgrade(&request).expect("prepare");
    inject_upgrade_fault(Some(UpgradeFault::LivePublished));
    activate_upgrade(&confirmed(&request.operation)).expect_err("fault");
    inject_upgrade_fault(None);
    let recovered = recover_upgrade(&confirmed(&request.operation)).expect("recover");
    assert_eq!(recovered.phase, UpgradePhase::Activated);
}

#[test]
fn upgrade_recover_refuses_tampered_candidate() {
    let (_directory, request, _) = fixture();
    prepare_upgrade(&request).expect("prepare");
    fs::write(request.operation.join("candidate.db"), b"tampered").expect("tamper");
    let error = recover_upgrade(&confirmed(&request.operation)).expect_err("tamper");
    assert!(error.to_string().contains("tampered"));
}

#[test]
fn upgrade_torn_finalized_record_refuses_rollback() {
    let (_directory, request, database) = fixture();
    let original = fs::read(&database).expect("original");
    prepare_upgrade(&request).expect("prepare");
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    finalize_upgrade(&confirmed(&request.operation)).expect("finalize");
    let mut journal: Vec<_> = fs::read_dir(request.operation.join("journal"))
        .expect("journal")
        .map(|entry| entry.expect("entry").path())
        .collect();
    journal.sort();
    let last = journal.last().expect("last");
    let bytes = fs::read(last).expect("bytes");
    fs::write(last, &bytes[..bytes.len() / 2]).expect("tear");
    rollback_upgrade(&confirmed(&request.operation)).expect_err("closed");
    recover_upgrade(&confirmed(&request.operation)).expect_err("torn");
    let status = upgrade_status(&request.operation).expect_err("torn status");
    assert!(status.to_string().contains("torn"));
    assert_ne!(fs::read(&database).expect("still candidate"), original);
}

#[test]
fn upgrade_recover_does_not_invent_prepare_after_activating_intent() {
    let (_directory, request, database) = fixture();
    let original = fs::read(&database).expect("original");
    prepare_upgrade(&request).expect("prepare");
    inject_upgrade_fault(Some(UpgradeFault::ActivatingRecorded));
    activate_upgrade(&confirmed(&request.operation)).expect_err("fault");
    inject_upgrade_fault(None);
    let recovered = recover_upgrade(&confirmed(&request.operation)).expect("recover");
    assert_eq!(recovered.phase, UpgradePhase::Prepared);
    assert_eq!(fs::read(&database).expect("unchanged"), original);
}

#[test]
fn upgrade_wal_writer_child() {
    let Ok(path) = env::var("ENGRAM_UPGRADE_TEST_WAL_LIVE") else {
        return;
    };
    let writer = Connection::open(path).expect("writer");
    writer
        .execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; PRAGMA user_version=2;",
        )
        .expect("committed WAL");
    std::mem::forget(writer);
    std::process::exit(73);
}

#[test]
fn upgrade_rollback_refuses_committed_wal_writes() {
    let (_directory, request, database) = fixture();
    prepare_upgrade(&request).expect("prepare");
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    let output = Command::new(env::current_exe().expect("test exe"))
        .args([
            "storage::migration::upgrade::tests::upgrade_wal_writer_child",
            "--exact",
            "--test-threads=1",
        ])
        .env("ENGRAM_UPGRADE_TEST_WAL_LIVE", &database)
        .output()
        .expect("wal child");
    assert_eq!(
        output.status.code(),
        Some(73),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let wal = PathBuf::from(format!("{}-wal", database.display()));
    assert!(fs::metadata(&wal).expect("wal").len() > 0);
    let error = rollback_upgrade(&confirmed(&request.operation)).expect_err("wal writes");
    assert!(error.to_string().contains("unknown writes"));
    assert!(wal.exists());
    assert!(request.operation.join("retained-original.db").exists());
}

#[test]
fn upgrade_journal_writer_child() {
    let Ok(path) = env::var("ENGRAM_UPGRADE_TEST_JOURNAL_LIVE") else {
        return;
    };
    let writer = Connection::open(&path).expect("writer");
    writer
        .execute_batch("PRAGMA journal_mode=DELETE; BEGIN; CREATE TABLE leftover_journal(x INTEGER); INSERT INTO leftover_journal VALUES (1);")
        .expect("open journal");
    std::mem::forget(writer);
    std::process::exit(73);
}

#[test]
fn upgrade_exit_hook_child() {
    let Ok(name) = env::var("ENGRAM_UPGRADE_TEST_EXIT_FAULT") else {
        return;
    };
    let operation = PathBuf::from(env::var("ENGRAM_UPGRADE_TEST_OPERATION").expect("operation"));
    let fault = match name.as_str() {
        "RetainedWal" => UpgradeFault::RetainedWal,
        "RetainedShm" => UpgradeFault::RetainedShm,
        "RetainedJournal" => UpgradeFault::RetainedJournal,
        "RetainedMain" => UpgradeFault::RetainedMain,
        "LiveStagingCopy" => UpgradeFault::LiveStagingCopy,
        "LiveStagingLinked" => UpgradeFault::LiveStagingLinked,
        "LiveStaged" => UpgradeFault::LiveStaged,
        "LiveLinked" => UpgradeFault::LiveLinked,
        "JournalRecordTempCreated" | "JournalRecordTempCreatedPrepare" => {
            UpgradeFault::JournalRecordTempCreated
        }
        "JournalRecordLinked" | "JournalRecordLinkedFinalize" | "JournalRecordLinkedPrepare" => {
            UpgradeFault::JournalRecordLinked
        }
        "RestoredWal" => UpgradeFault::RestoredWal,
        "RestoredShm" => UpgradeFault::RestoredShm,
        "RestoredJournal" => UpgradeFault::RestoredJournal,
        "RestoredMain" => UpgradeFault::RestoredMain,
        other => panic!("unknown fault {other}"),
    };
    inject_upgrade_exit(Some(fault));
    match name.as_str() {
        "RestoredWal" | "RestoredShm" | "RestoredJournal" | "RestoredMain" => {
            let _ = rollback_upgrade(&confirmed(&operation));
        }
        "JournalRecordTempCreatedPrepare" | "JournalRecordLinkedPrepare" => {
            let database =
                PathBuf::from(env::var("ENGRAM_UPGRADE_TEST_DATABASE").expect("database"));
            let old_executable =
                PathBuf::from(env::var("ENGRAM_UPGRADE_TEST_OLD_EXECUTABLE").expect("old"));
            let _ = prepare_upgrade(&UpgradePrepareRequest {
                database,
                operation,
                old_executable,
                offline_confirmed: true,
            });
        }
        "JournalRecordLinkedFinalize" => {
            let _ = finalize_upgrade(&confirmed(&operation));
        }
        _ => {
            let _ = activate_upgrade(&confirmed(&operation));
        }
    }
}

fn spawn_exit(fault: &str, operation: &Path) {
    let output = Command::new(env::current_exe().expect("test exe"))
        .args([
            "storage::migration::upgrade::tests::upgrade_exit_hook_child",
            "--exact",
            "--test-threads=1",
        ])
        .env("ENGRAM_UPGRADE_TEST_EXIT_FAULT", fault)
        .env("ENGRAM_UPGRADE_TEST_OPERATION", operation)
        .output()
        .expect("exit child");
    assert_eq!(
        output.status.code(),
        Some(73),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn spawn_exit_prepare(fault: &str, request: &UpgradePrepareRequest) {
    let output = Command::new(env::current_exe().expect("test exe"))
        .args([
            "storage::migration::upgrade::tests::upgrade_exit_hook_child",
            "--exact",
            "--test-threads=1",
        ])
        .env("ENGRAM_UPGRADE_TEST_EXIT_FAULT", fault)
        .env("ENGRAM_UPGRADE_TEST_OPERATION", &request.operation)
        .env("ENGRAM_UPGRADE_TEST_DATABASE", &request.database)
        .env(
            "ENGRAM_UPGRADE_TEST_OLD_EXECUTABLE",
            &request.old_executable,
        )
        .output()
        .expect("exit child");
    assert_eq!(
        output.status.code(),
        Some(73),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn upgrade_recover_after_process_exit_on_retain_main() {
    let (_directory, request, database) = fixture();
    let original = fs::read(&database).expect("original");
    prepare_upgrade(&request).expect("prepare");
    spawn_exit("RetainedMain", &request.operation);
    let recovered = recover_upgrade(&confirmed(&request.operation)).expect("recover");
    assert_eq!(recovered.phase, UpgradePhase::Activated);
    assert_ne!(fs::read(&database).expect("live"), original);
}

#[test]
fn upgrade_recover_after_process_exit_on_rollback_main() {
    let (_directory, request, database) = fixture();
    let original = fs::read(&database).expect("original");
    prepare_upgrade(&request).expect("prepare");
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    spawn_exit("RestoredMain", &request.operation);
    let recovered = recover_upgrade(&confirmed(&request.operation)).expect("recover");
    assert_eq!(recovered.phase, UpgradePhase::RolledBack);
    assert_eq!(fs::read(&database).expect("restored"), original);
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn optional_bytes(path: &Path) -> Option<Vec<u8>> {
    path.is_file().then(|| fs::read(path).expect("bytes"))
}

fn snapshot_durable(path: &Path) -> (Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>) {
    (
        fs::read(path).expect("main"),
        optional_bytes(&sidecar(path, "-wal")),
        optional_bytes(&sidecar(path, "-journal")),
    )
}

fn assert_durable(path: &Path, main: &[u8], wal: Option<&[u8]>, journal: Option<&[u8]>) {
    assert_eq!(fs::read(path).expect("main"), main);
    match wal {
        Some(bytes) => assert_eq!(fs::read(sidecar(path, "-wal")).expect("wal"), bytes),
        None => assert!(!sidecar(path, "-wal").exists()),
    }
    match journal {
        Some(bytes) => assert_eq!(fs::read(sidecar(path, "-journal")).expect("journal"), bytes),
        None => assert!(!sidecar(path, "-journal").exists()),
    }
}

fn journal_len(operation: &Path) -> usize {
    fs::read_dir(operation.join("journal")).map_or(0, Iterator::count)
}

fn activating_records(operation: &Path) -> usize {
    let mut count = 0;
    let Ok(entries) = fs::read_dir(operation.join("journal")) else {
        return 0;
    };
    for entry in entries {
        let path = entry.expect("entry").path();
        let text = fs::read_to_string(&path).expect("journal");
        if text.contains("\"kind\": \"activating\"") {
            count += 1;
        }
    }
    count
}

fn spawn_named_child(filter: &str, key: &str, value: &Path) {
    let output = Command::new(env::current_exe().expect("test exe"))
        .args([filter, "--exact", "--test-threads=1"])
        .env(key, value)
        .output()
        .expect("child");
    assert_eq!(
        output.status.code(),
        Some(73),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn plant_source_wal(database: &Path) {
    spawn_named_child(
        "storage::migration::upgrade::tests::upgrade_wal_writer_child",
        "ENGRAM_UPGRADE_TEST_WAL_LIVE",
        database,
    );
    assert!(sidecar(database, "-wal").is_file());
    assert!(sidecar(database, "-shm").is_file());
}

fn sidecar_fixture() -> (
    crate::test_support::TempHome,
    UpgradePrepareRequest,
    PathBuf,
) {
    let (directory, request, database) = fixture();
    plant_source_wal(&database);
    fs::write(sidecar(&database, "-journal"), b"leftover-journal-bytes").expect("journal leftover");
    (directory, request, database)
}

#[test]
fn upgrade_legal_transition_allows_activating_to_rolling_back() {
    assert!(legal_transition(
        Some(JournalKind::Activating),
        JournalKind::RollingBack
    ));
    assert!(legal_transition(
        Some(JournalKind::Activating),
        JournalKind::Activated
    ));
    assert!(!legal_transition(
        Some(JournalKind::Prepared),
        JournalKind::RollingBack
    ));
}

#[test]
fn upgrade_validate_journal_records_compares_entire_prepared_identity() {
    let record = JournalRecord {
        sequence: 1,
        kind: JournalKind::Prepared,
        database: PathBuf::from("store.db"),
        database_normalized: "store.db".into(),
        selected_executable: PathBuf::from("old"),
        backup_sha256: "b".into(),
        archive_sha256: "a".into(),
        candidate_sha256: "c".into(),
        selected_executable_sha256: "e".into(),
        current_executable: PathBuf::from("cur"),
        current_executable_sha256: "u".into(),
        live_main_sha256: "m".into(),
        live_wal_sha256: Some("w".into()),
        live_journal_sha256: None,
    };
    let mut next = record.clone();
    next.sequence = 2;
    next.kind = JournalKind::Activating;
    validate_journal_records(&[record.clone(), next.clone()]).expect("same identity");
    next.live_wal_sha256 = Some("changed".into());
    validate_journal_records(&[record, next]).expect_err("identity drift");
}

#[test]
fn upgrade_activate_retry_after_activating_record_is_idempotent() {
    let (_directory, request, _) = fixture();
    prepare_upgrade(&request).expect("prepare");
    inject_upgrade_fault(Some(UpgradeFault::ActivatingRecorded));
    activate_upgrade(&confirmed(&request.operation)).expect_err("fault");
    inject_upgrade_fault(None);
    assert_eq!(activating_records(&request.operation), 1);
    let activated = activate_upgrade(&confirmed(&request.operation)).expect("retry");
    assert_eq!(activated.phase, UpgradePhase::Activated);
    assert_eq!(activating_records(&request.operation), 1);
    assert_eq!(journal_len(&request.operation), 3);
}

#[test]
fn upgrade_rollback_from_activating_intent() {
    let (_directory, request, database) = fixture();
    let original = fs::read(&database).expect("original");
    prepare_upgrade(&request).expect("prepare");
    inject_upgrade_fault(Some(UpgradeFault::ActivatingRecorded));
    activate_upgrade(&confirmed(&request.operation)).expect_err("fault");
    inject_upgrade_fault(None);
    let rolled = rollback_upgrade(&confirmed(&request.operation)).expect("rollback");
    assert_eq!(rolled.phase, UpgradePhase::RolledBack);
    assert_eq!(fs::read(&database).expect("unchanged"), original);
}

#[test]
fn upgrade_prepare_refuses_operation_overlapping_database() {
    let directory = temp_home().expect("directory");
    let database = directory.path().join("store.db");
    drop(SqliteStore::open_unresolved(&database).expect("current store"));
    let executable = directory.path().join("old-bin");
    fs::write(&executable, b"compatible-old-bytes").expect("executable");
    let request = UpgradePrepareRequest {
        database: database.clone(),
        operation: database.join("inside"),
        old_executable: executable,
        offline_confirmed: true,
    };
    let before = directory_snapshot(directory.path());
    let error = prepare_upgrade(&request).expect_err("overlap");
    assert!(
        overlap_or_unix_not_a_directory(&error),
        "expected overlaps or Unix NotADirectory, got {error}"
    );
    assert_eq!(directory_snapshot(directory.path()), before);
}

#[test]
fn upgrade_prepare_refuses_database_inside_operation() {
    let directory = temp_home().expect("directory");
    let operation = directory.path().join("operation");
    fs::create_dir_all(&operation).expect("operation");
    let database = operation.join("store.db");
    drop(SqliteStore::open_unresolved(&database).expect("current store"));
    let executable = directory.path().join("old-bin");
    fs::write(&executable, b"compatible-old-bytes").expect("executable");
    let request = UpgradePrepareRequest {
        database,
        operation,
        old_executable: executable,
        offline_confirmed: true,
    };
    let error = prepare_upgrade(&request).expect_err("overlap");
    assert!(error.to_string().contains("overlaps"));
}

#[cfg(windows)]
#[test]
fn upgrade_prepare_refuses_junction_ancestor() {
    let directory = temp_home().expect("directory");
    let real = directory.path().join("real");
    fs::create_dir_all(&real).expect("real");
    let alias = directory.path().join("alias");
    let output = Command::new("cmd")
        .args([
            "/C",
            "mklink",
            "/J",
            alias.to_str().expect("alias"),
            real.to_str().expect("real"),
        ])
        .output()
        .expect("mklink /J");
    assert!(
        output.status.success(),
        "privilege-free junction is required for Windows reparse coverage: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let database = alias.join("store.db");
    drop(SqliteStore::open_unresolved(&database).expect("current store"));
    let executable = real.join("old-bin");
    fs::write(&executable, b"compatible-old-bytes").expect("executable");
    let request = UpgradePrepareRequest {
        database,
        operation: alias.join("operation"),
        old_executable: executable,
        offline_confirmed: true,
    };
    let error = prepare_upgrade(&request).expect_err("reparse");
    assert!(error.to_string().contains("reparse") || error.to_string().contains("symlink"));
}

#[cfg(unix)]
#[test]
fn upgrade_prepare_binds_posix_ancestor_alias() {
    let directory = temp_home().expect("directory");
    let real = directory.path().join("real");
    fs::create_dir_all(&real).expect("real");
    let alias = directory.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).expect("posix ancestor alias");
    let database = alias.join("store.db");
    drop(SqliteStore::open_unresolved(&database).expect("current store"));
    let executable = real.join("old-bin");
    fs::write(&executable, b"compatible-old-bytes").expect("executable");
    let request = UpgradePrepareRequest {
        database: database.clone(),
        operation: alias.join("operation"),
        old_executable: executable,
        offline_confirmed: true,
    };
    let prepared = prepare_upgrade(&request).expect("bind posix ancestor");
    assert_eq!(
        prepared.database,
        fs::canonicalize(real.join("store.db")).expect("real store")
    );
    assert_eq!(
        prepared.operation,
        fs::canonicalize(real.join("operation")).expect("real operation")
    );
    assert!(real.join("operation").join("backup.db").is_file());
}

#[test]
fn upgrade_prepare_fault_hooks_keep_partial_artifacts() {
    let cases = [
        (
            UpgradeFault::BackupPublished,
            &["backup.db"][..],
            &["archive.db", "candidate.db", "old-executable"][..],
        ),
        (
            UpgradeFault::ArchivePublished,
            &["backup.db", "archive.db"],
            &["candidate.db", "old-executable"],
        ),
        (
            UpgradeFault::CandidatePublished,
            &["backup.db", "archive.db", "candidate.db"],
            &["old-executable"],
        ),
        (
            UpgradeFault::ExecutableCopied,
            &["backup.db", "archive.db", "candidate.db", "old-executable"],
            &[],
        ),
    ];
    for (fault, present, absent) in cases {
        let (_directory, request, database) = fixture();
        let original = fs::read(&database).expect("original");
        inject_upgrade_fault(Some(fault));
        prepare_upgrade(&request).expect_err("fault");
        inject_upgrade_fault(None);
        for name in present {
            assert!(
                request.operation.join(name).is_file(),
                "{fault:?} should keep {name}"
            );
        }
        for name in absent {
            assert!(
                !request.operation.join(name).exists(),
                "{fault:?} should not have {name}"
            );
        }
        assert_eq!(journal_len(&request.operation), 0);
        let error = recover_upgrade(&confirmed(&request.operation)).expect_err("incomplete");
        assert!(error.to_string().contains("incomplete"));
        assert_eq!(fs::read(&database).expect("live"), original);
    }

    let (_directory, request, database) = fixture();
    let original = fs::read(&database).expect("original");
    inject_upgrade_fault(Some(UpgradeFault::PreparedRecorded));
    prepare_upgrade(&request).expect_err("fault");
    inject_upgrade_fault(None);
    assert_eq!(journal_len(&request.operation), 1);
    let recovered = recover_upgrade(&confirmed(&request.operation)).expect("recover");
    assert_eq!(recovered.phase, UpgradePhase::Prepared);
    assert_eq!(fs::read(&database).expect("live"), original);
}

#[test]
fn upgrade_activate_fault_hooks_and_recover() {
    let cases = [
        UpgradeFault::ActivatingRecorded,
        UpgradeFault::OriginalRetained,
        UpgradeFault::RetainedMain,
        UpgradeFault::LiveStaged,
        UpgradeFault::LiveLinked,
        UpgradeFault::LivePublished,
        UpgradeFault::ActivatedRecorded,
    ];
    for fault in cases {
        let (_directory, request, database) = fixture();
        prepare_upgrade(&request).expect("prepare");
        let (original, original_wal, original_journal) = snapshot_durable(&database);
        inject_upgrade_fault(Some(fault));
        activate_upgrade(&confirmed(&request.operation)).expect_err("fault");
        inject_upgrade_fault(None);
        match fault {
            UpgradeFault::ActivatingRecorded => {
                assert!(database.is_file());
                assert!(!request.operation.join("retained-original.db").exists());
                let recovered = recover_upgrade(&confirmed(&request.operation)).expect("recover");
                assert_eq!(recovered.phase, UpgradePhase::Prepared);
                assert_eq!(fs::read(&database).expect("live"), original);
            }
            UpgradeFault::ActivatedRecorded => {
                let recovered = recover_upgrade(&confirmed(&request.operation)).expect("recover");
                assert_eq!(recovered.phase, UpgradePhase::Activated);
                assert_ne!(fs::read(&database).expect("live"), original);
                assert_durable(
                    &request.operation.join("retained-original.db"),
                    &original,
                    original_wal.as_deref(),
                    original_journal.as_deref(),
                );
            }
            _ => {
                let recovered = recover_upgrade(&confirmed(&request.operation)).expect("recover");
                assert_eq!(
                    recovered.phase,
                    UpgradePhase::Activated,
                    "recover after {fault:?}"
                );
                assert_ne!(fs::read(&database).expect("live"), original);
                assert_durable(
                    &request.operation.join("retained-original.db"),
                    &original,
                    original_wal.as_deref(),
                    original_journal.as_deref(),
                );
            }
        }
    }
}

#[test]
fn upgrade_sidecar_retain_faults_reconcile_per_component() {
    let cases = [
        UpgradeFault::RetainedWal,
        UpgradeFault::RetainedShm,
        UpgradeFault::RetainedJournal,
    ];
    for fault in cases {
        let (_directory, request, database) = sidecar_fixture();
        prepare_upgrade(&request).expect("prepare");
        let (original, original_wal, original_journal) = snapshot_durable(&database);
        inject_upgrade_fault(Some(fault));
        activate_upgrade(&confirmed(&request.operation)).expect_err("fault");
        inject_upgrade_fault(None);
        assert!(database.is_file(), "{fault:?} must keep live main");
        match fault {
            UpgradeFault::RetainedWal => {
                assert!(sidecar(&request.operation.join("retained-original.db"), "-wal").is_file());
                assert!(!sidecar(&database, "-wal").exists());
            }
            UpgradeFault::RetainedJournal => {
                assert!(
                    sidecar(&request.operation.join("retained-original.db"), "-journal").is_file()
                );
                assert!(!sidecar(&database, "-journal").exists());
            }
            UpgradeFault::RetainedShm => {
                assert!(sidecar(&request.operation.join("retained-original.db"), "-shm").is_file());
            }
            _ => {}
        }
        let recovered = recover_upgrade(&confirmed(&request.operation)).expect("recover");
        assert_eq!(
            recovered.phase,
            UpgradePhase::Activated,
            "recover after {fault:?}"
        );
        assert_ne!(fs::read(&database).expect("live"), original);
        assert_durable(
            &request.operation.join("retained-original.db"),
            &original,
            original_wal.as_deref(),
            original_journal.as_deref(),
        );
    }
}

#[test]
fn upgrade_rollback_fault_hooks_and_recover() {
    let cases = [
        UpgradeFault::RestoredWal,
        UpgradeFault::RestoredShm,
        UpgradeFault::RestoredJournal,
        UpgradeFault::RestoredMain,
        UpgradeFault::RollbackMoved,
    ];
    for fault in cases {
        let (_directory, request, database) = sidecar_fixture();
        prepare_upgrade(&request).expect("prepare");
        let (original, original_wal, original_journal) = snapshot_durable(&database);
        activate_upgrade(&confirmed(&request.operation)).expect("activate");
        inject_upgrade_fault(Some(fault));
        rollback_upgrade(&confirmed(&request.operation)).expect_err("fault");
        inject_upgrade_fault(None);
        match fault {
            UpgradeFault::RestoredWal => {
                assert!(sidecar(&database, "-wal").is_file());
                assert!(request.operation.join("retained-original.db").is_file());
                assert!(!database.exists());
            }
            UpgradeFault::RestoredMain | UpgradeFault::RollbackMoved => {
                assert_eq!(fs::read(&database).expect("restored"), original);
            }
            _ => {}
        }
        let recovered = recover_upgrade(&confirmed(&request.operation)).expect("recover");
        assert_eq!(
            recovered.phase,
            UpgradePhase::RolledBack,
            "recover after {fault:?}"
        );
        assert_durable(
            &database,
            &original,
            original_wal.as_deref(),
            original_journal.as_deref(),
        );
    }
}

#[test]
fn upgrade_partial_restore_does_not_delete_restored_wal() {
    let (_directory, request, database) = sidecar_fixture();
    let original = fs::read(&database).expect("original");
    prepare_upgrade(&request).expect("prepare");
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    inject_upgrade_fault(Some(UpgradeFault::RestoredWal));
    rollback_upgrade(&confirmed(&request.operation)).expect_err("fault");
    inject_upgrade_fault(None);
    let restored_wal = fs::read(sidecar(&database, "-wal")).expect("wal");
    let recovered = recover_upgrade(&confirmed(&request.operation)).expect("recover");
    assert_eq!(recovered.phase, UpgradePhase::RolledBack);
    assert_eq!(fs::read(&database).expect("restored"), original);
    assert_eq!(
        fs::read(sidecar(&database, "-wal")).expect("wal"),
        restored_wal
    );
}

#[test]
fn upgrade_finalize_fault_hook_and_recover() {
    let (_directory, request, _) = fixture();
    prepare_upgrade(&request).expect("prepare");
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    inject_upgrade_fault(Some(UpgradeFault::FinalizedRecorded));
    finalize_upgrade(&confirmed(&request.operation)).expect_err("fault");
    inject_upgrade_fault(None);
    let recovered = recover_upgrade(&confirmed(&request.operation)).expect("recover");
    assert_eq!(recovered.phase, UpgradePhase::Finalized);
}

#[test]
fn upgrade_process_exit_on_destructive_boundaries() {
    let activate_exits = [
        "RetainedWal",
        "RetainedShm",
        "RetainedJournal",
        "RetainedMain",
        "LiveStagingCopy",
        "LiveStagingLinked",
        "LiveStaged",
        "LiveLinked",
    ];
    for fault in activate_exits {
        let (_directory, request, database) = sidecar_fixture();
        let original = fs::read(&database).expect("original");
        prepare_upgrade(&request).expect("prepare");
        spawn_exit(fault, &request.operation);
        let recovered = recover_upgrade(&confirmed(&request.operation)).expect(fault);
        assert_eq!(
            recovered.phase,
            UpgradePhase::Activated,
            "exit recover {fault}"
        );
        assert_ne!(fs::read(&database).expect("live"), original);
    }
    let rollback_exits = [
        "RestoredWal",
        "RestoredShm",
        "RestoredJournal",
        "RestoredMain",
    ];
    for fault in rollback_exits {
        let (_directory, request, database) = sidecar_fixture();
        let original = fs::read(&database).expect("original");
        prepare_upgrade(&request).expect("prepare");
        activate_upgrade(&confirmed(&request.operation)).expect("activate");
        spawn_exit(fault, &request.operation);
        let recovered = recover_upgrade(&confirmed(&request.operation)).expect(fault);
        assert_eq!(
            recovered.phase,
            UpgradePhase::RolledBack,
            "exit recover {fault}"
        );
        assert_eq!(fs::read(&database).expect("restored"), original);
    }
}

#[test]
fn upgrade_wrong_retained_refuses_before_reserved_temp_cleanup() {
    let (_directory, request, database) = sidecar_fixture();
    prepare_upgrade(&request).expect("prepare");
    inject_upgrade_fault(Some(UpgradeFault::RetainedWal));
    activate_upgrade(&confirmed(&request.operation)).expect_err("fault");
    inject_upgrade_fault(None);
    let retained_main = request.operation.join("retained-original.db");
    fs::write(&retained_main, b"wrong-retained-main").expect("wrong retained");
    let prepared: JournalRecord = serde_json::from_slice(
        &fs::read(request.operation.join("journal").join("00000001.json")).expect("prepared"),
    )
    .expect("decode");
    let mut next = prepared;
    next.sequence = 3;
    next.kind = JournalKind::Activated;
    let bytes = serde_json::to_vec_pretty(&next).expect("bytes");
    let temp = request.operation.join("journal-publish.tmp");
    fs::write(&temp, &bytes[..bytes.len() / 2]).expect("journal temp prefix");
    let before = directory_snapshot(request.operation.parent().expect("parent"));
    let error = recover_upgrade(&confirmed(&request.operation)).expect_err("contradiction");
    assert!(
        error.to_string().contains("cannot reconcile"),
        "actual error: {error}"
    );
    activate_upgrade(&confirmed(&request.operation)).expect_err("activate");
    assert_eq!(
        directory_snapshot(request.operation.parent().expect("parent")),
        before
    );
    assert!(temp.is_file(), "journal temp kept before retain cleanup");
    assert_eq!(
        fs::read(&retained_main).expect("kept"),
        b"wrong-retained-main"
    );
    assert!(database.is_file());
}

#[test]
fn upgrade_retain_refuses_unknown_live_bytes_when_component_already_retained() {
    let (_directory, request, database) = sidecar_fixture();
    prepare_upgrade(&request).expect("prepare");
    inject_upgrade_fault(Some(UpgradeFault::RetainedWal));
    activate_upgrade(&confirmed(&request.operation)).expect_err("fault");
    inject_upgrade_fault(None);
    fs::write(sidecar(&database, "-wal"), b"unknown-live-wal").expect("unknown wal");
    let error = recover_upgrade(&confirmed(&request.operation)).expect_err("contradiction");
    assert!(
        error.to_string().contains("live and retained")
            || error.to_string().contains("cannot reconcile")
    );
    assert_eq!(
        fs::read(sidecar(&database, "-wal")).expect("kept"),
        b"unknown-live-wal"
    );
    assert!(sidecar(&request.operation.join("retained-original.db"), "-wal").is_file());
}

#[test]
fn upgrade_activate_after_live_publication_records_activated() {
    let (_directory, request, _) = fixture();
    prepare_upgrade(&request).expect("prepare");
    inject_upgrade_fault(Some(UpgradeFault::LivePublished));
    activate_upgrade(&confirmed(&request.operation)).expect_err("fault");
    inject_upgrade_fault(None);
    let activated = activate_upgrade(&confirmed(&request.operation)).expect("activate");
    assert_eq!(activated.phase, UpgradePhase::Activated);
    let kinds = fs::read_dir(request.operation.join("journal"))
        .expect("journal")
        .map(|entry| fs::read_to_string(entry.expect("entry").path()).expect("record"))
        .collect::<Vec<_>>();
    assert_eq!(
        kinds
            .iter()
            .filter(|text| text.contains("\"kind\": \"activated\""))
            .count(),
        1
    );
}

#[test]
fn upgrade_finalize_after_live_publication_records_legal_journal() {
    let (_directory, request, _) = fixture();
    prepare_upgrade(&request).expect("prepare");
    inject_upgrade_fault(Some(UpgradeFault::LivePublished));
    activate_upgrade(&confirmed(&request.operation)).expect_err("fault");
    inject_upgrade_fault(None);
    let finalized = finalize_upgrade(&confirmed(&request.operation)).expect("finalize");
    assert_eq!(finalized.phase, UpgradePhase::Finalized);
    upgrade_status(&request.operation).expect("legal status");
    let kinds = fs::read_dir(request.operation.join("journal"))
        .expect("journal")
        .map(|entry| fs::read_to_string(entry.expect("entry").path()).expect("record"))
        .collect::<Vec<_>>();
    assert_eq!(
        kinds
            .iter()
            .filter(|text| text.contains("\"kind\": \"activated\""))
            .count(),
        1
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|text| text.contains("\"kind\": \"finalized\""))
            .count(),
        1
    );
}

fn directory_snapshot(root: &Path) -> Vec<(PathBuf, SnapshotKind)> {
    fn visit(root: &Path, path: &Path, entries: &mut Vec<(PathBuf, SnapshotKind)>) {
        let mut children: Vec<_> = fs::read_dir(path)
            .expect("inventory")
            .map(|entry| entry.expect("entry").path())
            .collect();
        children.sort();
        for path in children {
            let relative = path.strip_prefix(root).expect("owned").to_path_buf();
            let metadata = fs::symlink_metadata(&path).expect("metadata");
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                entries.push((
                    relative,
                    SnapshotKind::Symlink(fs::read_link(&path).expect("link")),
                ));
                continue;
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                const REPARSE_POINT: u32 = 0x400;
                if metadata.file_attributes() & REPARSE_POINT != 0 {
                    entries.push((relative, SnapshotKind::Other));
                    continue;
                }
            }
            if file_type.is_dir() {
                entries.push((relative, SnapshotKind::Directory));
                visit(root, &path, entries);
            } else if file_type.is_file() {
                entries.push((
                    relative,
                    SnapshotKind::File(fs::read(&path).expect("bytes")),
                ));
            } else {
                entries.push((relative, SnapshotKind::Other));
            }
        }
    }
    let mut entries = Vec::new();
    visit(root, root, &mut entries);
    entries
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum SnapshotKind {
    Directory,
    File(Vec<u8>),
    Symlink(PathBuf),
    Other,
}

fn plant_equal_content_candidate(request: &UpgradePrepareRequest, database: &Path) {
    let original = fs::read(database).expect("original");
    fs::write(request.operation.join("candidate.db"), &original).expect("equal candidate");
    let journal = request.operation.join("journal").join("00000001.json");
    let mut record: serde_json::Value =
        serde_json::from_slice(&fs::read(&journal).expect("journal")).expect("json");
    record["candidate_sha256"] = record["live_main_sha256"].clone();
    fs::write(
        &journal,
        serde_json::to_vec_pretty(&record).expect("patched journal"),
    )
    .expect("write journal");
}

fn append_rolling_back(operation: &Path) {
    let mut records: Vec<_> = fs::read_dir(operation.join("journal"))
        .expect("journal")
        .map(|entry| entry.expect("entry").path())
        .collect();
    records.sort();
    let mut intent: serde_json::Value =
        serde_json::from_slice(&fs::read(records.last().expect("last")).expect("record"))
            .expect("json");
    assert_eq!(intent["kind"], "activated");
    let sequence = intent["sequence"].as_u64().expect("sequence") + 1;
    intent["sequence"] = sequence.into();
    intent["kind"] = "rolling_back".into();
    fs::write(
        operation
            .join("journal")
            .join(format!("{sequence:08}.json")),
        serde_json::to_vec_pretty(&intent).expect("intent"),
    )
    .expect("rolling_back");
}

fn publish_staging(operation: &Path) -> PathBuf {
    operation.join("publish-staging")
}

fn current_import_fixture() -> (
    crate::test_support::TempHome,
    UpgradePrepareRequest,
    PathBuf,
) {
    let directory = temp_home().expect("directory");
    let seed = directory.path().join("seed.db");
    drop(SqliteStore::open_unresolved(&seed).expect("seed"));
    let archive = directory.path().join("seed-archive.db");
    crate::storage::migration::export_store(&seed, &archive).expect("export");
    let first = directory.path().join("imported-once.db");
    crate::storage::migration::import_archive(&archive, &first).expect("first import");
    let archive_again = directory.path().join("imported-archive.db");
    crate::storage::migration::export_store(&first, &archive_again).expect("re-export");
    let database = directory.path().join("store.db");
    crate::storage::migration::import_archive(&archive_again, &database)
        .expect("fixed-point import");
    assert_eq!(
        fs::read(&first).expect("first"),
        fs::read(&database).expect("fixed-point"),
        "current DELETE-mode import must be a fixed point"
    );
    let executable = directory.path().join("old-bin");
    fs::write(&executable, b"compatible-old-bytes").expect("executable");
    let request = UpgradePrepareRequest {
        database: database.clone(),
        operation: directory.path().join("operation"),
        old_executable: executable,
        offline_confirmed: true,
    };
    (directory, request, database)
}

fn remove_last_journal(operation: &Path) {
    let mut records: Vec<_> = fs::read_dir(operation.join("journal"))
        .expect("journal")
        .map(|entry| entry.expect("entry").path())
        .collect();
    records.sort();
    fs::remove_file(records.last().expect("last")).expect("drop completion record");
}

#[test]
fn upgrade_phase_rolling_back_serializes_as_snake_case() {
    assert_eq!(
        serde_json::to_value(UpgradePhase::RollingBack).expect("json"),
        "rolling_back"
    );
}

#[test]
fn upgrade_prepare_captures_source_identities_before_backup() {
    let (_directory, request, database) = sidecar_fixture();
    let (main, wal, journal) = snapshot_durable(&database);
    let prepared = prepare_upgrade(&request).expect("prepare");
    let record: serde_json::Value = serde_json::from_slice(
        &fs::read(request.operation.join("journal").join("00000001.json")).expect("journal"),
    )
    .expect("json");
    assert_eq!(prepared.phase, UpgradePhase::Prepared);
    assert_eq!(
        record["live_main_sha256"].as_str(),
        Some(format!("{:x}", Sha256::digest(&main)).as_str())
    );
    assert_eq!(
        record["live_wal_sha256"].as_str(),
        Some(format!("{:x}", Sha256::digest(wal.expect("wal"))).as_str())
    );
    assert_eq!(
        record["live_journal_sha256"].as_str(),
        Some(format!("{:x}", Sha256::digest(journal.expect("journal"))).as_str())
    );
}

#[test]
fn upgrade_prepare_refuses_reserved_sidecar_operation_paths_before_effects() {
    let suffixes: &[&str] = &["-wal", "-shm", "-journal", "-WAL", "-SHM", "-JOURNAL"];
    for suffix in suffixes {
        for descendant in [false, true] {
            let directory = temp_home().expect("directory");
            let database = directory.path().join("store.db");
            drop(SqliteStore::open_unresolved(&database).expect("current store"));
            let executable = directory.path().join("old-bin");
            fs::write(&executable, b"compatible-old-bytes").expect("executable");
            let reserved = sidecar(&database, suffix);
            assert!(!reserved.exists(), "absent reserved path");
            let operation = if descendant {
                reserved.join("operation")
            } else {
                reserved
            };
            let before = directory_snapshot(directory.path());
            let request = UpgradePrepareRequest {
                database: database.clone(),
                operation,
                old_executable: executable,
                offline_confirmed: true,
            };
            let error = prepare_upgrade(&request).expect_err("overlap");
            assert!(error.to_string().contains("overlaps"));
            assert_eq!(
                directory_snapshot(directory.path()),
                before,
                "reserved suffix {suffix}, descendant {descendant}"
            );
        }
    }
}

#[test]
fn upgrade_prepare_refuses_parentdir_spelling_of_reserved_sidecar() {
    let directory = temp_home().expect("directory");
    let database = directory.path().join("store.db");
    drop(SqliteStore::open_unresolved(&database).expect("current store"));
    let executable = directory.path().join("old-bin");
    fs::write(&executable, b"compatible-old-bytes").expect("executable");
    let mut reserved_name = database.file_name().expect("name").to_os_string();
    reserved_name.push("-wal");
    let operation = directory
        .path()
        .join("missing")
        .join("..")
        .join(reserved_name);
    let before = directory_snapshot(directory.path());
    let request = UpgradePrepareRequest {
        database,
        operation,
        old_executable: executable,
        offline_confirmed: true,
    };
    let error = prepare_upgrade(&request).expect_err("parentdir");
    assert!(
        error.to_string().contains("overlaps") || error.to_string().contains("parent directory")
    );
    assert_eq!(directory_snapshot(directory.path()), before);
}

#[test]
fn upgrade_prepare_refuses_when_source_changes_before_prepared() {
    let (_directory, request, database) = fixture();
    let original = fs::read(&database).expect("original");
    inject_prepare_source_mutation(true);
    let error = prepare_upgrade(&request).expect_err("changed");
    inject_prepare_source_mutation(false);
    assert!(error.to_string().contains("changed"));
    let after = fs::read(&database).expect("mutated source");
    assert_ne!(after, original);
    assert_eq!(*after.last().expect("marker"), 0x5a);
    assert!(
        !request
            .operation
            .join("journal")
            .join("00000001.json")
            .exists()
    );
}

#[test]
fn upgrade_split_activation_then_interrupted_rollback_recovers() {
    let (_directory, request, database) = sidecar_fixture();
    let (original, original_wal, original_journal) = snapshot_durable(&database);
    prepare_upgrade(&request).expect("prepare");
    inject_upgrade_fault(Some(UpgradeFault::RetainedJournal));
    activate_upgrade(&confirmed(&request.operation)).expect_err("partial activate");
    inject_upgrade_fault(None);
    assert!(database.is_file(), "main stays live");
    assert!(sidecar(&request.operation.join("retained-original.db"), "-wal").is_file());
    assert!(sidecar(&request.operation.join("retained-original.db"), "-journal").is_file());
    inject_upgrade_fault(Some(UpgradeFault::RestoredWal));
    rollback_upgrade(&confirmed(&request.operation)).expect_err("partial restore");
    inject_upgrade_fault(None);
    assert!(database.is_file(), "split original main remains");
    assert!(sidecar(&database, "-wal").is_file(), "restored wal remains");
    assert!(
        sidecar(&request.operation.join("retained-original.db"), "-journal").is_file(),
        "journal remains retained at recover"
    );
    let recovered = recover_upgrade(&confirmed(&request.operation)).expect("recover");
    assert_eq!(recovered.phase, UpgradePhase::RolledBack);
    assert_durable(
        &database,
        &original,
        original_wal.as_deref(),
        original_journal.as_deref(),
    );
}

#[test]
fn upgrade_equal_content_from_current_import_fixed_point() {
    let (_directory, request, database) = current_import_fixture();
    let original = fs::read(&database).expect("original");
    let prepared = prepare_upgrade(&request).expect("prepare");
    let candidate = fs::read(request.operation.join("candidate.db")).expect("candidate");
    assert_eq!(
        original, candidate,
        "upgrade candidate must match the imported DELETE-mode current store"
    );
    assert_eq!(
        prepared.candidate_sha256,
        prepared_report_live_main(&request)
    );
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    inject_upgrade_fault(Some(UpgradeFault::RestoredMain));
    rollback_upgrade(&confirmed(&request.operation)).expect_err("restored main");
    inject_upgrade_fault(None);
    assert_eq!(fs::read(&database).expect("already restored"), original);
    let recovered = recover_upgrade(&confirmed(&request.operation)).expect("recover");
    assert_eq!(recovered.phase, UpgradePhase::RolledBack);
    assert_eq!(fs::read(&database).expect("kept original"), original);
}

#[test]
fn upgrade_equal_content_restore_does_not_delete_restored_main() {
    let (_directory, request, database) = fixture();
    let original = fs::read(&database).expect("original");
    let prepared = prepare_upgrade(&request).expect("prepare");
    let candidate = fs::read(request.operation.join("candidate.db")).expect("candidate");
    if original == candidate {
        assert_eq!(
            prepared.candidate_sha256,
            prepared_report_live_main(&request)
        );
    } else {
        plant_equal_content_candidate(&request, &database);
    }
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    inject_upgrade_fault(Some(UpgradeFault::RestoredMain));
    rollback_upgrade(&confirmed(&request.operation)).expect_err("restored main");
    inject_upgrade_fault(None);
    assert_eq!(fs::read(&database).expect("already restored"), original);
    let recovered = recover_upgrade(&confirmed(&request.operation)).expect("recover");
    assert_eq!(recovered.phase, UpgradePhase::RolledBack);
    assert_eq!(fs::read(&database).expect("kept original"), original);
}

#[test]
fn upgrade_c5_activating_intent_before_retain_equal_content_keeps_original() {
    let (_directory, request, database) = current_import_fixture();
    prepare_upgrade(&request).expect("prepare");
    let original = fs::read(&database).expect("original");
    let candidate = fs::read(request.operation.join("candidate.db")).expect("candidate");
    assert_eq!(
        original, candidate,
        "current import fixture must keep equal original and candidate bytes"
    );
    inject_upgrade_fault(Some(UpgradeFault::ActivatingRecorded));
    activate_upgrade(&confirmed(&request.operation)).expect_err("fault");
    inject_upgrade_fault(None);
    assert!(
        !request.operation.join("retained-original.db").exists(),
        "ActivatingRecorded must fire before retention"
    );
    assert_eq!(fs::read(&database).expect("untouched live"), original);
    let recovered = recover_upgrade(&confirmed(&request.operation)).expect("recover");
    assert_eq!(recovered.phase, UpgradePhase::Prepared);
    assert_eq!(
        fs::read(&database).expect("recover kept original"),
        original
    );
    let rolled = rollback_upgrade(&confirmed(&request.operation)).expect("rollback");
    assert_eq!(rolled.phase, UpgradePhase::RolledBack);
    assert_eq!(
        fs::read(&database).expect("rollback kept original"),
        original
    );
}

fn prepared_report_live_main(request: &UpgradePrepareRequest) -> String {
    let record: serde_json::Value = serde_json::from_slice(
        &fs::read(request.operation.join("journal").join("00000001.json")).expect("journal"),
    )
    .expect("json");
    record["live_main_sha256"]
        .as_str()
        .expect("live main")
        .to_owned()
}

#[test]
fn upgrade_restore_refuses_missing_retained_on_activating_without_effects() {
    let (_directory, request, database) = fixture();
    prepare_upgrade(&request).expect("prepare");
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    remove_last_journal(&request.operation);
    fs::remove_file(request.operation.join("retained-original.db")).expect("drop retained");
    let before = directory_snapshot(request.operation.parent().expect("parent"));
    rollback_upgrade(&confirmed(&request.operation)).expect_err("rollback");
    recover_upgrade(&confirmed(&request.operation)).expect_err("recover");
    activate_upgrade(&confirmed(&request.operation)).expect_err("activate");
    finalize_upgrade(&confirmed(&request.operation)).expect_err("finalize");
    assert_eq!(
        directory_snapshot(request.operation.parent().expect("parent")),
        before
    );
    let kinds = fs::read_dir(request.operation.join("journal"))
        .expect("journal")
        .map(|entry| fs::read_to_string(entry.expect("entry").path()).expect("record"))
        .collect::<Vec<_>>();
    assert!(kinds.iter().all(|text| !text.contains("rolling_back")));
    assert!(database.is_file());
}

#[test]
fn upgrade_publish_staging_is_operation_owned() {
    let (_directory, request, database) = fixture();
    let original = fs::read(&database).expect("original");
    prepare_upgrade(&request).expect("prepare");
    let candidate = fs::read(request.operation.join("candidate.db")).expect("candidate");
    let staged = publish_staging(&request.operation);
    fs::write(&staged, &candidate).expect("pre-existing same-byte staging");
    let before = directory_snapshot(request.operation.parent().expect("parent"));
    let error = activate_upgrade(&confirmed(&request.operation)).expect_err("pre-existing");
    assert!(
        error.to_string().contains("staging") || error.to_string().contains("refuse before retain")
    );
    assert_eq!(
        directory_snapshot(request.operation.parent().expect("parent")),
        before
    );
    assert_eq!(fs::read(&database).expect("live"), original);
    assert_eq!(fs::read(&staged).expect("kept planted"), candidate);

    fs::remove_file(&staged).expect("clear plant");
    inject_upgrade_fault(Some(UpgradeFault::LiveStaged));
    activate_upgrade(&confirmed(&request.operation)).expect_err("staged");
    inject_upgrade_fault(None);
    assert!(staged.is_file(), "operation-owned staging remains");
    assert!(!database.exists());
    let recovered = recover_upgrade(&confirmed(&request.operation)).expect("recover");
    assert_eq!(recovered.phase, UpgradePhase::Activated);
    assert!(database.is_file());
    assert!(!staged.exists(), "owned staging removed after LiveLinked");
}

#[test]
fn upgrade_rolling_back_status_refuses_activate_and_finalize_before_effects() {
    let (_directory, request, database) = fixture();
    let original = fs::read(&database).expect("original");
    prepare_upgrade(&request).expect("prepare");
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    append_rolling_back(&request.operation);
    let before = directory_snapshot(request.operation.parent().expect("parent"));
    assert_eq!(
        upgrade_status(&request.operation).expect("status").phase,
        UpgradePhase::RollingBack
    );
    activate_upgrade(&confirmed(&request.operation)).expect_err("activate");
    finalize_upgrade(&confirmed(&request.operation)).expect_err("finalize");
    assert_eq!(
        directory_snapshot(request.operation.parent().expect("parent")),
        before
    );
    let recovered = recover_upgrade(&confirmed(&request.operation)).expect("recover");
    assert_eq!(recovered.phase, UpgradePhase::RolledBack);
    assert_eq!(fs::read(&database).expect("restored"), original);
}

#[test]
fn upgrade_rolled_back_stays_terminal_after_live_resume() {
    let (_directory, request, database) = fixture();
    prepare_upgrade(&request).expect("prepare");
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    rollback_upgrade(&confirmed(&request.operation)).expect("rollback");
    let mut bytes = fs::read(&database).expect("restored");
    bytes.push(b'X');
    fs::write(&database, &bytes).expect("resume");
    assert_eq!(
        upgrade_status(&request.operation).expect("status").phase,
        UpgradePhase::RolledBack
    );
    assert_eq!(
        recover_upgrade(&confirmed(&request.operation))
            .expect("recover")
            .phase,
        UpgradePhase::RolledBack
    );
    assert_eq!(fs::read(&database).expect("left resumed"), bytes);
    activate_upgrade(&confirmed(&request.operation)).expect_err("activate");
    finalize_upgrade(&confirmed(&request.operation)).expect_err("finalize");
    assert_eq!(fs::read(&database).expect("still resumed"), bytes);
}

#[test]
fn upgrade_rollback_refuses_unexpected_live_wal_before_effects() {
    let (_directory, request, database) = fixture();
    prepare_upgrade(&request).expect("prepare");
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    let live = fs::read(&database).expect("candidate");
    fs::write(sidecar(&database, "-wal"), b"unexpected-wal").expect("wal");
    let before = directory_snapshot(request.operation.parent().expect("parent"));
    let error = rollback_upgrade(&confirmed(&request.operation)).expect_err("unexpected wal");
    assert!(error.to_string().contains("unexpected") || error.to_string().contains("unknown"));
    assert_eq!(
        directory_snapshot(request.operation.parent().expect("parent")),
        before
    );
    assert_eq!(fs::read(&database).expect("kept candidate"), live);
}

#[test]
fn upgrade_rollback_reconciles_owned_publish_staging() {
    let (_directory, request, database) = fixture();
    let original = fs::read(&database).expect("original");
    prepare_upgrade(&request).expect("prepare");
    inject_upgrade_fault(Some(UpgradeFault::LiveLinked));
    activate_upgrade(&confirmed(&request.operation)).expect_err("linked");
    inject_upgrade_fault(None);
    let staged = publish_staging(&request.operation);
    assert!(staged.is_file());
    assert!(database.is_file());
    let rolled = rollback_upgrade(&confirmed(&request.operation)).expect("rollback");
    assert_eq!(rolled.phase, UpgradePhase::RolledBack);
    assert_eq!(fs::read(&database).expect("restored"), original);
    assert!(!staged.exists(), "owned staging removed on rollback");
}

#[test]
fn upgrade_finalize_reconciles_owned_publish_staging_after_live_linked() {
    let (_directory, request, database) = fixture();
    prepare_upgrade(&request).expect("prepare");
    inject_upgrade_fault(Some(UpgradeFault::LiveLinked));
    activate_upgrade(&confirmed(&request.operation)).expect_err("linked");
    inject_upgrade_fault(None);
    let staged = publish_staging(&request.operation);
    assert!(staged.is_file());
    let finalized = finalize_upgrade(&confirmed(&request.operation)).expect("finalize");
    assert_eq!(finalized.phase, UpgradePhase::Finalized);
    assert!(database.is_file());
    assert!(!staged.exists(), "owned staging removed on finalize");
}

#[test]
fn upgrade_finalize_refuses_foreign_publish_staging_before_effects() {
    let (_directory, request, database) = fixture();
    prepare_upgrade(&request).expect("prepare");
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    let staged = publish_staging(&request.operation);
    fs::write(&staged, b"foreign-staging").expect("foreign");
    let before = directory_snapshot(request.operation.parent().expect("parent"));
    finalize_upgrade(&confirmed(&request.operation)).expect_err("foreign");
    assert_eq!(
        directory_snapshot(request.operation.parent().expect("parent")),
        before
    );
    assert_eq!(fs::read(&staged).expect("kept"), b"foreign-staging");
    assert!(database.is_file());
}

#[cfg(unix)]
#[test]
fn upgrade_activate_refuses_dangling_publish_staging_before_retain() {
    let (_directory, request, database) = fixture();
    let original = fs::read(&database).expect("original");
    prepare_upgrade(&request).expect("prepare");
    let staged = publish_staging(&request.operation);
    std::os::unix::fs::symlink("missing-staging-target", &staged).expect("dangling alias");
    let before = directory_snapshot(request.operation.parent().expect("parent"));
    let error = activate_upgrade(&confirmed(&request.operation)).expect_err("dangling");
    assert!(error.to_string().contains("staging"));
    assert_eq!(
        directory_snapshot(request.operation.parent().expect("parent")),
        before
    );
    assert_eq!(fs::read(&database).expect("live"), original);
}

#[cfg(windows)]
#[test]
fn upgrade_prepare_refuses_unicode_case_reserved_sidecar_before_effects() {
    let directory = temp_home().expect("directory");
    let database = directory.path().join("é.db");
    drop(SqliteStore::open_unresolved(&database).expect("current store"));
    let executable = directory.path().join("old-bin");
    fs::write(&executable, b"compatible-old-bytes").expect("executable");
    let cases = [
        directory.path().join("É.db-wal"),
        directory.path().join("É.db-wal").join("operation"),
        directory.path().join("É.db"),
    ];
    for operation in cases {
        let before = directory_snapshot(directory.path());
        let request = UpgradePrepareRequest {
            database: database.clone(),
            operation: operation.clone(),
            old_executable: executable.clone(),
            offline_confirmed: true,
        };
        let error = prepare_upgrade(&request).expect_err("unicode overlap");
        assert!(
            error.to_string().contains("overlaps"),
            "{}: {error}",
            operation.display()
        );
        assert_eq!(
            directory_snapshot(directory.path()),
            before,
            "{}",
            operation.display()
        );
    }
}

#[cfg(windows)]
#[test]
fn upgrade_overlap_comparator_ambiguous_non_ascii_and_ascii_distinct() {
    assert!(
        super::path_prefix_overlap(Path::new(r"C:\tmp\café.db"), Path::new(r"C:\tmp\cafe.db")),
        "same-directory non-ASCII vs ASCII is ambiguous overlap"
    );
    assert!(
        super::path_prefix_overlap(Path::new(r"C:\tmp\é.db-wal"), Path::new(r"C:\tmp\É.db-wal")),
        "same-directory non-ASCII pair is ambiguous overlap"
    );
    assert!(
        super::path_prefix_overlap(
            Path::new(r"C:\tmp\STORE.DB-wal"),
            Path::new(r"C:\tmp\store.db-wal")
        ),
        "both-ASCII ignore-case still overlaps"
    );
    assert!(
        !super::path_prefix_overlap(Path::new(r"C:\alpha\é.db"), Path::new(r"C:\beta\upgrade")),
        "ASCII-distinct ancestor proves paths are not aliases"
    );
}

#[cfg(windows)]
#[test]
fn upgrade_prepare_allows_ascii_distinct_sibling_of_unicode_database() {
    let directory = temp_home().expect("directory");
    let data = directory.path().join("data");
    let ops = directory.path().join("ops");
    fs::create_dir_all(&data).expect("data");
    let database = data.join("é.db");
    drop(SqliteStore::open_unresolved(&database).expect("current store"));
    let executable = directory.path().join("old-bin");
    fs::write(&executable, b"compatible-old-bytes").expect("executable");
    let request = UpgradePrepareRequest {
        database,
        operation: ops.join("upgrade"),
        old_executable: executable,
        offline_confirmed: true,
    };
    let prepared = prepare_upgrade(&request).expect("ascii-distinct sibling");
    assert_eq!(prepared.phase, UpgradePhase::Prepared);
}

#[test]
fn upgrade_c6_overlap_comparator_ambiguous_unicode_nfc_and_ascii_case() {
    assert!(
        super::path_prefix_overlap(Path::new("tmp/é.db-wal"), Path::new("tmp/É.db-wal")),
        "Unicode case pair is ambiguous overlap on every OS"
    );
    assert!(
        super::path_prefix_overlap(Path::new("tmp/é.db-wal"), Path::new("tmp/e\u{301}.db-wal")),
        "NFC versus NFD is ambiguous overlap on every OS"
    );
    assert!(
        super::path_prefix_overlap(Path::new("tmp/store.db-wal"), Path::new("tmp/STORE.DB-WAL")),
        "reserved sidecar ASCII case overlaps on every OS"
    );
    assert!(
        !super::path_prefix_overlap(Path::new("alpha/é.db"), Path::new("beta/upgrade")),
        "ASCII-distinct ancestor proves paths are not aliases"
    );
}

#[test]
fn upgrade_c6_prepare_refuses_ambiguous_unicode_sidecar_paths_before_effects() {
    for alias in ["É.db-wal", "e\u{301}.db-wal"] {
        for descendant in [false, true] {
            let directory = temp_home().expect("directory");
            let database = directory.path().join("é.db");
            drop(SqliteStore::open_unresolved(&database).expect("current store"));
            let executable = directory.path().join("old-bin");
            fs::write(&executable, b"compatible-old-bytes").expect("executable");
            let reserved = directory.path().join(alias);
            assert!(!reserved.exists(), "reserved sidecar must be absent");
            let operation = if descendant {
                reserved.join("operation")
            } else {
                reserved
            };
            let before = directory_snapshot(directory.path());
            let request = UpgradePrepareRequest {
                database: database.clone(),
                operation,
                old_executable: executable,
                offline_confirmed: true,
            };
            let error = prepare_upgrade(&request).expect_err("unicode overlap");
            assert!(
                error.to_string().contains("overlaps"),
                "{alias} descendant={descendant}: {error}"
            );
            assert_eq!(
                directory_snapshot(directory.path()),
                before,
                "{alias} descendant={descendant}"
            );
        }
    }
}

#[test]
fn upgrade_activate_refuses_preexisting_partial_before_retain() {
    let (_directory, request, database) = fixture();
    let original = fs::read(&database).expect("original");
    prepare_upgrade(&request).expect("prepare");
    let partial = request.operation.join("publish-staging.partial");
    fs::write(&partial, b"planted-partial").expect("partial");
    let unknown = request
        .operation
        .parent()
        .expect("parent")
        .join("unknown-bytes");
    fs::write(&unknown, b"keep-me").expect("unknown");
    let before = directory_snapshot(request.operation.parent().expect("parent"));
    let error = activate_upgrade(&confirmed(&request.operation)).expect_err("preexisting");
    assert!(
        error.to_string().contains("staging") || error.to_string().contains("refuse before retain")
    );
    assert_eq!(
        directory_snapshot(request.operation.parent().expect("parent")),
        before
    );
    assert_eq!(fs::read(&database).expect("live"), original);
    assert_eq!(fs::read(&partial).expect("kept"), b"planted-partial");
    assert_eq!(fs::read(&unknown).expect("unknown"), b"keep-me");
}

#[test]
fn upgrade_mid_copy_child_exit_recovers_activate_and_rollback() {
    let (_directory, request, database) = fixture();
    let original = fs::read(&database).expect("original");
    prepare_upgrade(&request).expect("prepare");
    let unknown = request
        .operation
        .parent()
        .expect("parent")
        .join("unknown-bytes");
    fs::write(&unknown, b"keep-me").expect("unknown");
    spawn_exit("LiveStagingCopy", &request.operation);
    let partial = request.operation.join("publish-staging.partial");
    assert!(partial.is_file(), "incomplete copy remains");
    assert_eq!(fs::read(&unknown).expect("unknown"), b"keep-me");
    let recovered = recover_upgrade(&confirmed(&request.operation)).expect("recover");
    assert_eq!(recovered.phase, UpgradePhase::Activated);
    assert_ne!(fs::read(&database).expect("live"), original);
    assert!(!partial.exists(), "accounted partial removed");
    assert_eq!(fs::read(&unknown).expect("unknown"), b"keep-me");

    let (_directory, request, database) = fixture();
    let original = fs::read(&database).expect("original");
    prepare_upgrade(&request).expect("prepare");
    spawn_exit("LiveStagingCopy", &request.operation);
    let rolled = rollback_upgrade(&confirmed(&request.operation)).expect("rollback");
    assert_eq!(rolled.phase, UpgradePhase::RolledBack);
    assert_eq!(fs::read(&database).expect("restored"), original);
    assert!(!request.operation.join("publish-staging.partial").exists());
}

#[test]
fn upgrade_non_candidate_partial_is_preserved() {
    let (_directory, request, database) = fixture();
    let original = fs::read(&database).expect("original");
    prepare_upgrade(&request).expect("prepare");
    spawn_exit("LiveStagingCopy", &request.operation);
    let partial = request.operation.join("publish-staging.partial");
    fs::write(&partial, b"not-a-candidate-prefix").expect("corrupt");
    let unknown = request
        .operation
        .parent()
        .expect("parent")
        .join("unknown-bytes");
    fs::write(&unknown, b"keep-me").expect("unknown");
    let before = directory_snapshot(request.operation.parent().expect("parent"));
    recover_upgrade(&confirmed(&request.operation)).expect_err("recover");
    activate_upgrade(&confirmed(&request.operation)).expect_err("activate");
    rollback_upgrade(&confirmed(&request.operation)).expect_err("rollback");
    assert_eq!(
        directory_snapshot(request.operation.parent().expect("parent")),
        before
    );
    assert_eq!(fs::read(&partial).expect("kept"), b"not-a-candidate-prefix");
    assert_eq!(fs::read(&unknown).expect("unknown"), b"keep-me");
    assert_eq!(
        fs::read(request.operation.join("retained-original.db")).expect("retained"),
        original
    );
}

#[test]
fn upgrade_staging_hard_link_child_exit_drops_alias() {
    let (_directory, request, database) = fixture();
    let original = fs::read(&database).expect("original");
    prepare_upgrade(&request).expect("prepare");
    spawn_exit("LiveStagingLinked", &request.operation);
    let staged = publish_staging(&request.operation);
    let partial = request.operation.join("publish-staging.partial");
    assert!(staged.is_file());
    assert!(partial.is_file(), "extra name remains until recover");
    let recovered = recover_upgrade(&confirmed(&request.operation)).expect("recover");
    assert_eq!(recovered.phase, UpgradePhase::Activated);
    assert_ne!(fs::read(&database).expect("live"), original);
    assert!(!partial.exists());
    assert!(!staged.exists());
}

#[test]
fn upgrade_journal_temp_prepare_first_preserves_files() {
    let (_directory, request, database) = fixture();
    let original = fs::read(&database).expect("original");
    spawn_exit_prepare("JournalRecordTempCreatedPrepare", &request);
    let temp = request.operation.join("journal-publish.tmp");
    assert!(temp.is_file(), "interrupted prepare must leave a temp");
    assert!(
        fs::metadata(&temp).expect("temp len").len() > 0,
        "child exit after a nonempty slice"
    );
    let before = directory_snapshot(request.operation.parent().expect("parent"));
    let error = recover_upgrade(&confirmed(&request.operation)).expect_err("incomplete");
    assert!(error.to_string().contains("incomplete prepare"));
    assert_eq!(
        directory_snapshot(request.operation.parent().expect("parent")),
        before
    );
    assert!(temp.is_file(), "prepared-first temp preserved");
    assert_eq!(fs::read(&database).expect("live"), original);
}

#[test]
fn upgrade_journal_temp_later_transition_is_recoverable() {
    let (_directory, request, database) = fixture();
    let original = fs::read(&database).expect("original");
    prepare_upgrade(&request).expect("prepare");
    spawn_exit("JournalRecordTempCreated", &request.operation);
    let temp = request.operation.join("journal-publish.tmp");
    let recovered = recover_upgrade(&confirmed(&request.operation)).expect("recover");
    assert_eq!(recovered.phase, UpgradePhase::Prepared);
    assert!(!temp.exists(), "later unique temp discarded");
    assert_eq!(fs::read(&database).expect("live"), original);
    let activated = activate_upgrade(&confirmed(&request.operation)).expect("activate");
    assert_eq!(activated.phase, UpgradePhase::Activated);
}

#[test]
fn upgrade_journal_temp_survives_corrupted_candidate_refusal() {
    let (_directory, request, _) = fixture();
    prepare_upgrade(&request).expect("prepare");
    spawn_exit("JournalRecordTempCreated", &request.operation);
    let temp = request.operation.join("journal-publish.tmp");
    assert!(temp.is_file());
    fs::write(request.operation.join("candidate.db"), b"corrupt-candidate").expect("corrupt");
    let before = directory_snapshot(request.operation.parent().expect("parent"));
    recover_upgrade(&confirmed(&request.operation)).expect_err("recover");
    activate_upgrade(&confirmed(&request.operation)).expect_err("activate");
    finalize_upgrade(&confirmed(&request.operation)).expect_err("finalize");
    assert_eq!(
        directory_snapshot(request.operation.parent().expect("parent")),
        before
    );
    assert!(temp.is_file(), "temp kept when artifacts fail");
}

#[test]
fn upgrade_unexpected_live_wal_preserves_publish_partial() {
    let (_directory, request, database) = fixture();
    prepare_upgrade(&request).expect("prepare");
    spawn_exit("LiveStagingCopy", &request.operation);
    let partial = request.operation.join("publish-staging.partial");
    assert!(partial.is_file());
    fs::write(sidecar(&database, "-wal"), b"unexpected-wal").expect("wal");
    let before = directory_snapshot(request.operation.parent().expect("parent"));
    recover_upgrade(&confirmed(&request.operation)).expect_err("recover");
    activate_upgrade(&confirmed(&request.operation)).expect_err("activate");
    rollback_upgrade(&confirmed(&request.operation)).expect_err("rollback");
    finalize_upgrade(&confirmed(&request.operation)).expect_err("finalize");
    assert_eq!(
        directory_snapshot(request.operation.parent().expect("parent")),
        before
    );
    assert!(
        partial.is_file(),
        "partial kept when live sidecar is unexpected"
    );
}

#[test]
fn upgrade_journal_link_child_exit_drops_alias() {
    let (_directory, request, database) = fixture();
    let original = fs::read(&database).expect("original");
    prepare_upgrade(&request).expect("prepare");
    spawn_exit("JournalRecordLinked", &request.operation);
    let temp = request.operation.join("journal-publish.tmp");
    assert!(
        request
            .operation
            .join("journal")
            .join("00000002.json")
            .is_file(),
        "published activating record"
    );
    let recovered = recover_upgrade(&confirmed(&request.operation)).expect("recover");
    assert_eq!(recovered.phase, UpgradePhase::Prepared);
    assert!(!temp.exists(), "hard-link alias unlinked");
    assert_eq!(fs::read(&database).expect("live"), original);
    let activated = activate_upgrade(&confirmed(&request.operation)).expect("activate");
    assert_eq!(activated.phase, UpgradePhase::Activated);
}

#[test]
fn upgrade_journal_link_child_exit_after_activated_drops_alias() {
    let (_directory, request, database) = fixture();
    let original = fs::read(&database).expect("original");
    prepare_upgrade(&request).expect("prepare");
    inject_upgrade_fault(Some(UpgradeFault::LivePublished));
    activate_upgrade(&confirmed(&request.operation)).expect_err("fault");
    inject_upgrade_fault(None);
    spawn_exit("JournalRecordLinked", &request.operation);
    let temp = request.operation.join("journal-publish.tmp");
    assert!(
        request
            .operation
            .join("journal")
            .join("00000003.json")
            .is_file(),
        "published activated record"
    );
    assert!(temp.is_file(), "extra name remains until recover");
    let recovered = recover_upgrade(&confirmed(&request.operation)).expect("recover");
    assert_eq!(recovered.phase, UpgradePhase::Activated);
    assert!(!temp.exists(), "hard-link alias unlinked");
    assert_ne!(fs::read(&database).expect("live"), original);
}

#[test]
fn upgrade_journal_link_child_exit_after_finalized_drops_alias() {
    let (_directory, request, database) = fixture();
    prepare_upgrade(&request).expect("prepare");
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    spawn_exit("JournalRecordLinkedFinalize", &request.operation);
    let temp = request.operation.join("journal-publish.tmp");
    assert!(
        request
            .operation
            .join("journal")
            .join("00000004.json")
            .is_file(),
        "published finalized record"
    );
    assert!(temp.is_file(), "extra name remains until recover");
    let recovered = recover_upgrade(&confirmed(&request.operation)).expect("recover");
    assert_eq!(recovered.phase, UpgradePhase::Finalized);
    assert!(!temp.exists(), "hard-link alias unlinked");
    assert!(database.is_file());
}

#[test]
fn upgrade_reserved_temps_contradiction_preserves_both() {
    let (_directory, request, _) = fixture();
    prepare_upgrade(&request).expect("prepare");
    spawn_exit("LiveStagingCopy", &request.operation);
    let partial = request.operation.join("publish-staging.partial");
    let temp = request.operation.join("journal-publish.tmp");
    assert!(partial.is_file());
    fs::write(&partial, b"not-a-candidate-prefix").expect("corrupt partial");
    let prepared: JournalRecord = serde_json::from_slice(
        &fs::read(request.operation.join("journal").join("00000001.json")).expect("prepared"),
    )
    .expect("decode");
    let mut next = prepared;
    next.sequence = 3;
    next.kind = JournalKind::Activated;
    let bytes = serde_json::to_vec_pretty(&next).expect("bytes");
    fs::write(&temp, &bytes[..bytes.len() / 2]).expect("journal temp prefix");
    let before = directory_snapshot(request.operation.parent().expect("parent"));
    recover_upgrade(&confirmed(&request.operation)).expect_err("recover");
    activate_upgrade(&confirmed(&request.operation)).expect_err("activate");
    rollback_upgrade(&confirmed(&request.operation)).expect_err("rollback");
    assert_eq!(
        directory_snapshot(request.operation.parent().expect("parent")),
        before
    );
    assert!(
        temp.is_file(),
        "legal journal temp kept with contradictory partial"
    );
    assert_eq!(
        fs::read(&partial).expect("partial"),
        b"not-a-candidate-prefix"
    );
}

fn prepared_journal(operation: &Path) -> JournalRecord {
    serde_json::from_slice(
        &fs::read(operation.join("journal").join("00000001.json")).expect("prepared"),
    )
    .expect("decode")
}

fn plant_legal_journal_temp(operation: &Path, sequence: u32, kind: JournalKind) -> PathBuf {
    let mut record = prepared_journal(operation);
    record.sequence = sequence;
    record.kind = kind;
    let bytes = serde_json::to_vec_pretty(&record).expect("bytes");
    let temp = operation.join("journal-publish.tmp");
    fs::write(&temp, &bytes[..bytes.len() / 2]).expect("temp prefix");
    temp
}

fn plant_foreign_partial(operation: &Path) -> PathBuf {
    let partial = operation.join("publish-staging.partial");
    fs::write(&partial, b"foreign-staging-partial").expect("foreign partial");
    partial
}

fn refuse_mutating_without_effects(operation: &Path) {
    let parent = operation.parent().expect("parent");
    let before = directory_snapshot(parent);
    recover_upgrade(&confirmed(operation)).expect_err("recover");
    assert_eq!(directory_snapshot(parent), before, "after recover");
    activate_upgrade(&confirmed(operation)).expect_err("activate");
    assert_eq!(directory_snapshot(parent), before, "after activate");
    rollback_upgrade(&confirmed(operation)).expect_err("rollback");
    assert_eq!(directory_snapshot(parent), before, "after rollback");
    finalize_upgrade(&confirmed(operation)).expect_err("finalize");
    assert_eq!(directory_snapshot(parent), before, "after finalize");
}

fn refuse_without_effects(operation: &Path) {
    refuse_mutating_without_effects(operation);
    let parent = operation.parent().expect("parent");
    let before = directory_snapshot(parent);
    upgrade_status(operation).expect_err("status");
    assert_eq!(directory_snapshot(parent), before, "after status");
}

fn load_journal_records(operation: &Path) -> Vec<JournalRecord> {
    let mut paths: Vec<_> = fs::read_dir(operation.join("journal"))
        .expect("journal")
        .map(|entry| entry.expect("entry").path())
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            serde_json::from_slice(&fs::read(&path).expect("record")).expect("decode journal")
        })
        .collect()
}

fn fault_hook_row(fault: UpgradeFault) -> &'static str {
    match fault {
        UpgradeFault::BackupPublished
        | UpgradeFault::ArchivePublished
        | UpgradeFault::CandidatePublished
        | UpgradeFault::ExecutableCopied
        | UpgradeFault::PreparedRecorded => "T1/C4",
        UpgradeFault::ActivatingRecorded => "T2/C4",
        UpgradeFault::OriginalRetained
        | UpgradeFault::RetainedWal
        | UpgradeFault::RetainedShm
        | UpgradeFault::RetainedJournal
        | UpgradeFault::RetainedMain => "T3/C4",
        UpgradeFault::LiveStagingCopy
        | UpgradeFault::LiveStagingLinked
        | UpgradeFault::LiveStaged
        | UpgradeFault::LiveLinked
        | UpgradeFault::LivePublished => "T3-T4/C4",
        UpgradeFault::JournalRecordTempCreated | UpgradeFault::JournalRecordLinked => "T1-T8/C4",
        UpgradeFault::ActivatedRecorded => "T4/C4",
        UpgradeFault::RollbackMoved
        | UpgradeFault::RestoredWal
        | UpgradeFault::RestoredShm
        | UpgradeFault::RestoredJournal
        | UpgradeFault::RestoredMain => "T5-T6/C4",
        UpgradeFault::FinalizedRecorded => "T7/C4",
    }
}

#[test]
fn upgrade_fault_hooks_map_onto_state_machine_rows() {
    assert_eq!(
        fault_hook_row(UpgradeFault::BackupPublished),
        "T1/C4",
        "T1 incomplete prepare"
    );
    assert_eq!(fault_hook_row(UpgradeFault::ActivatingRecorded), "T2/C4");
    assert_eq!(fault_hook_row(UpgradeFault::RetainedMain), "T3/C4");
    assert_eq!(fault_hook_row(UpgradeFault::LivePublished), "T3-T4/C4");
    assert_eq!(
        fault_hook_row(UpgradeFault::JournalRecordLinked),
        "T1-T8/C4"
    );
    assert_eq!(fault_hook_row(UpgradeFault::ActivatedRecorded), "T4/C4");
    assert_eq!(fault_hook_row(UpgradeFault::RestoredMain), "T5-T6/C4");
    assert_eq!(fault_hook_row(UpgradeFault::FinalizedRecorded), "T7/C4");
}

#[test]
fn upgrade_c1_prepared_wrong_retained_with_live_wal_refuses_before_effects() {
    let (_directory, request, _) = sidecar_fixture();
    prepare_upgrade(&request).expect("prepare");
    fs::write(
        request.operation.join("retained-original.db"),
        b"wrong-retained-main",
    )
    .expect("wrong retained");
    assert!(
        sidecar(&request.database, "-wal").is_file(),
        "live WAL remains after prepare"
    );
    refuse_without_effects(&request.operation);
    assert_eq!(activating_records(&request.operation), 0);
    assert_eq!(
        fs::read(request.operation.join("retained-original.db")).expect("kept"),
        b"wrong-retained-main"
    );
}

#[test]
fn upgrade_c1_t10_activating_intent_foreign_retained_wal_refuses_before_effects() {
    for suffix in ["-wal", "-journal"] {
        let (_directory, request, database) = fixture();
        prepare_upgrade(&request).expect("prepare");
        inject_upgrade_fault(Some(UpgradeFault::ActivatingRecorded));
        activate_upgrade(&confirmed(&request.operation)).expect_err("fault");
        inject_upgrade_fault(None);
        assert!(
            !sidecar(&database, suffix).exists(),
            "prepared original {suffix} must be absent"
        );
        let retained = request
            .operation
            .join(format!("retained-original.db{suffix}"));
        fs::write(&retained, b"foreign-retained-sidecar").expect("foreign retained sidecar");
        refuse_mutating_without_effects(&request.operation);
        assert_eq!(
            fs::read(&retained).expect("kept"),
            b"foreign-retained-sidecar"
        );
        let kinds = fs::read_dir(request.operation.join("journal"))
            .expect("journal")
            .map(|entry| fs::read_to_string(entry.expect("entry").path()).expect("record"))
            .collect::<Vec<_>>();
        assert!(
            kinds.iter().all(|text| !text.contains("rolling_back")),
            "{suffix}"
        );
        assert!(
            kinds.iter().all(|text| !text.contains("rolled_back")),
            "{suffix}"
        );
    }
}

#[test]
fn upgrade_c1_activated_corrupt_retained_refuses_before_effects() {
    let (_directory, request, database) = sidecar_fixture();
    prepare_upgrade(&request).expect("prepare");
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    let retained = request.operation.join("retained-original.db");
    fs::write(&retained, b"corrupt-retained-main").expect("corrupt retained");
    refuse_without_effects(&request.operation);
    assert_eq!(fs::read(&retained).expect("kept"), b"corrupt-retained-main");
    assert!(database.is_file());
    let kinds = fs::read_dir(request.operation.join("journal"))
        .expect("journal")
        .map(|entry| fs::read_to_string(entry.expect("entry").path()).expect("record"))
        .collect::<Vec<_>>();
    assert_eq!(
        kinds
            .iter()
            .filter(|text| text.contains("\"kind\": \"activated\""))
            .count(),
        1
    );
    assert!(kinds.iter().all(|text| !text.contains("rolling_back")));
    assert!(kinds.iter().all(|text| !text.contains("finalized")));
}

#[test]
fn upgrade_c2_activated_legal_journal_temp_foreign_partial_preserves_both() {
    let (_directory, request, _) = fixture();
    prepare_upgrade(&request).expect("prepare");
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    let temp = plant_legal_journal_temp(&request.operation, 4, JournalKind::Finalized);
    let partial = plant_foreign_partial(&request.operation);
    refuse_mutating_without_effects(&request.operation);
    assert!(temp.is_file(), "legal journal temp kept");
    assert_eq!(
        fs::read(&partial).expect("partial"),
        b"foreign-staging-partial"
    );
}

#[test]
fn upgrade_c2_activated_legal_journal_temp_foreign_staging_preserves_both() {
    let (_directory, request, _) = fixture();
    prepare_upgrade(&request).expect("prepare");
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    let temp = plant_legal_journal_temp(&request.operation, 4, JournalKind::Finalized);
    let staged = publish_staging(&request.operation);
    fs::write(&staged, b"foreign-full-staging").expect("foreign staging");
    refuse_mutating_without_effects(&request.operation);
    assert!(temp.is_file(), "legal journal temp kept");
    assert_eq!(fs::read(&staged).expect("staging"), b"foreign-full-staging");
}

#[test]
fn upgrade_c2_live_linked_legal_journal_temp_foreign_partial_preserves_both() {
    let (_directory, request, _) = fixture();
    prepare_upgrade(&request).expect("prepare");
    inject_upgrade_fault(Some(UpgradeFault::LivePublished));
    activate_upgrade(&confirmed(&request.operation)).expect_err("fault");
    inject_upgrade_fault(None);
    let temp = plant_legal_journal_temp(&request.operation, 3, JournalKind::Activated);
    let partial = plant_foreign_partial(&request.operation);
    refuse_mutating_without_effects(&request.operation);
    assert!(temp.is_file(), "legal journal temp kept");
    assert_eq!(
        fs::read(&partial).expect("partial"),
        b"foreign-staging-partial"
    );
}

#[test]
fn upgrade_c2_live_linked_legal_journal_temp_foreign_staging_preserves_both() {
    let (_directory, request, _) = fixture();
    prepare_upgrade(&request).expect("prepare");
    inject_upgrade_fault(Some(UpgradeFault::LivePublished));
    activate_upgrade(&confirmed(&request.operation)).expect_err("fault");
    inject_upgrade_fault(None);
    let temp = plant_legal_journal_temp(&request.operation, 3, JournalKind::Activated);
    let staged = publish_staging(&request.operation);
    fs::write(&staged, b"foreign-full-staging").expect("foreign staging");
    refuse_mutating_without_effects(&request.operation);
    assert!(temp.is_file(), "legal journal temp kept");
    assert_eq!(fs::read(&staged).expect("staging"), b"foreign-full-staging");
}

#[test]
fn upgrade_c2_split_retain_legal_journal_temp_foreign_partial_preserves_both() {
    let (_directory, request, _) = sidecar_fixture();
    prepare_upgrade(&request).expect("prepare");
    inject_upgrade_fault(Some(UpgradeFault::RetainedWal));
    activate_upgrade(&confirmed(&request.operation)).expect_err("fault");
    inject_upgrade_fault(None);
    let temp = plant_legal_journal_temp(&request.operation, 3, JournalKind::Activated);
    let partial = plant_foreign_partial(&request.operation);
    refuse_mutating_without_effects(&request.operation);
    assert!(temp.is_file(), "legal journal temp kept");
    assert_eq!(
        fs::read(&partial).expect("partial"),
        b"foreign-staging-partial"
    );
}

#[test]
fn upgrade_c2_t9_prepared_legal_journal_temp_foreign_staging_preserves_both() {
    let (_directory, request, _) = fixture();
    prepare_upgrade(&request).expect("prepare");
    let temp = plant_legal_journal_temp(&request.operation, 2, JournalKind::Activating);
    let staged = publish_staging(&request.operation);
    fs::write(&staged, b"foreign-full-staging").expect("foreign staging");
    refuse_mutating_without_effects(&request.operation);
    assert!(temp.is_file(), "legal journal temp kept");
    assert_eq!(fs::read(&staged).expect("staging"), b"foreign-full-staging");
}

#[test]
fn upgrade_c2_t10_activating_intent_legal_journal_temp_foreign_staging_preserves_both() {
    let (_directory, request, _) = fixture();
    prepare_upgrade(&request).expect("prepare");
    inject_upgrade_fault(Some(UpgradeFault::ActivatingRecorded));
    activate_upgrade(&confirmed(&request.operation)).expect_err("fault");
    inject_upgrade_fault(None);
    let temp = plant_legal_journal_temp(&request.operation, 3, JournalKind::RollingBack);
    let staged = publish_staging(&request.operation);
    fs::write(&staged, b"foreign-full-staging").expect("foreign staging");
    refuse_mutating_without_effects(&request.operation);
    assert!(temp.is_file(), "legal journal temp kept");
    assert_eq!(fs::read(&staged).expect("staging"), b"foreign-full-staging");
}

#[test]
fn upgrade_c3_empty_wal_copy_and_partial_cleanup_on_rollback() {
    let (_directory, request, database) = fixture();
    fs::write(sidecar(&database, "-wal"), b"").expect("empty wal");
    let original = fs::read(&database).expect("original");
    prepare_upgrade(&request).expect("prepare");
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    let candidate = fs::read(request.operation.join("candidate.db")).expect("candidate");
    let partial = request.operation.join("publish-staging.partial");
    fs::write(&partial, &candidate).expect("complete owned partial");
    let rolled = rollback_upgrade(&confirmed(&request.operation)).expect("rollback");
    assert_eq!(rolled.phase, UpgradePhase::RolledBack);
    assert_eq!(fs::read(&database).expect("restored"), original);
    assert_eq!(
        fs::read(sidecar(&database, "-wal")).expect("empty wal restored"),
        b""
    );
    assert!(!partial.exists(), "complete partial cleaned on rollback");
}

#[cfg(unix)]
#[test]
fn upgrade_c6_prepare_refuses_operation_leaf_symlink_before_effects() {
    let directory = temp_home().expect("directory");
    let database = directory.path().join("store.db");
    drop(SqliteStore::open_unresolved(&database).expect("current store"));
    let executable = directory.path().join("old-bin");
    fs::write(&executable, b"compatible-old-bytes").expect("executable");
    let real = directory.path().join("real-op");
    fs::create_dir_all(&real).expect("real");
    let operation = directory.path().join("operation");
    std::os::unix::fs::symlink(&real, &operation).expect("leaf symlink");
    let before = directory_snapshot(directory.path());
    let request = UpgradePrepareRequest {
        database,
        operation,
        old_executable: executable,
        offline_confirmed: true,
    };
    let error = prepare_upgrade(&request).expect_err("leaf alias");
    assert!(
        error.to_string().contains("symlink")
            || error.to_string().contains("reparse")
            || error.to_string().contains("alias"),
        "actual error: {error}"
    );
    assert_eq!(directory_snapshot(directory.path()), before);
    assert!(!real.join("backup.db").exists());
}

#[cfg(unix)]
#[test]
fn upgrade_c6_prepare_refuses_database_leaf_symlink_before_effects() {
    let directory = temp_home().expect("directory");
    let real_db = directory.path().join("real.db");
    drop(SqliteStore::open_unresolved(&real_db).expect("current store"));
    let database = directory.path().join("store.db");
    std::os::unix::fs::symlink(&real_db, &database).expect("db symlink");
    let executable = directory.path().join("old-bin");
    fs::write(&executable, b"compatible-old-bytes").expect("executable");
    let operation = directory.path().join("operation");
    let before = directory_snapshot(directory.path());
    let request = UpgradePrepareRequest {
        database,
        operation,
        old_executable: executable,
        offline_confirmed: true,
    };
    let error = prepare_upgrade(&request).expect_err("db alias");
    assert!(
        error.to_string().contains("symlink")
            || error.to_string().contains("reparse")
            || error.to_string().contains("alias"),
        "actual error: {error}"
    );
    assert_eq!(directory_snapshot(directory.path()), before);
    assert!(!request.operation.join("backup.db").exists());
}

#[test]
fn upgrade_c9_activated_original_live_without_retained_refuses_before_effects() {
    let (_directory, request, database) = fixture();
    let original = fs::read(&database).expect("original");
    prepare_upgrade(&request).expect("prepare");
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    validate_journal_records(&load_journal_records(&request.operation)).expect("legal journal");
    fs::remove_file(&database).expect("drop candidate live");
    fs::rename(request.operation.join("retained-original.db"), &database)
        .expect("restore original into live");
    validate_journal_records(&load_journal_records(&request.operation))
        .expect("legal journal after hand restore");
    assert!(!request.operation.join("retained-original.db").exists());
    refuse_without_effects(&request.operation);
    assert_eq!(fs::read(&database).expect("unchanged"), original);
    assert!(!request.operation.join("retained-original.db").exists());
    let kinds = fs::read_dir(request.operation.join("journal"))
        .expect("journal")
        .map(|entry| fs::read_to_string(entry.expect("entry").path()).expect("record"))
        .collect::<Vec<_>>();
    assert_eq!(
        kinds
            .iter()
            .filter(|text| text.contains("\"kind\": \"activated\""))
            .count(),
        1
    );
}

#[test]
fn upgrade_review_v5_t6_c1_restored_wal_corrupt_retained_refuses_before_effects() {
    let (_directory, request, database) = sidecar_fixture();
    let original_wal = fs::read(sidecar(&database, "-wal")).expect("original wal");
    prepare_upgrade(&request).expect("prepare");
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    inject_upgrade_fault(Some(UpgradeFault::RestoredWal));
    rollback_upgrade(&confirmed(&request.operation)).expect_err("restored wal");
    inject_upgrade_fault(None);
    assert_eq!(
        fs::read(sidecar(&database, "-wal")).expect("original wal live"),
        original_wal
    );
    let retained_wal = sidecar(&request.operation.join("retained-original.db"), "-wal");
    fs::write(&retained_wal, b"corrupt-retained-wal").expect("corrupt retained wal");
    refuse_mutating_without_effects(&request.operation);
    assert_eq!(
        fs::read(&retained_wal).expect("kept corrupt retained wal"),
        b"corrupt-retained-wal"
    );
    assert_eq!(
        fs::read(sidecar(&database, "-wal")).expect("live wal unchanged"),
        original_wal
    );
}

#[test]
fn upgrade_review_v5_t6_c1_restored_main_corrupt_retained_refuses_before_effects() {
    let (_directory, request, database) = fixture();
    prepare_upgrade(&request).expect("prepare");
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    inject_upgrade_fault(Some(UpgradeFault::RestoredMain));
    rollback_upgrade(&confirmed(&request.operation)).expect_err("restored main");
    inject_upgrade_fault(None);
    let original = fs::read(&database).expect("original live");
    let retained = request.operation.join("retained-original.db");
    fs::write(&retained, b"corrupt-retained-main").expect("corrupt retained main");
    refuse_mutating_without_effects(&request.operation);
    assert_eq!(
        fs::read(&retained).expect("kept corrupt retained main"),
        b"corrupt-retained-main"
    );
    assert_eq!(fs::read(&database).expect("live main unchanged"), original);
}

#[test]
fn upgrade_review_v5_t3_c5_retained_shm_equal_main_completes_by_location() {
    let (_directory, request, database, original) = interrupt_equal_content_at_retained_shm();
    assert_eq!(fs::read(&database).expect("untouched main"), original);
    assert!(
        !request.operation.join("retained-original.db").exists(),
        "main must still be live, not retained"
    );

    let (_directory, request, database, original) = interrupt_equal_content_at_retained_shm();
    let recovered = recover_upgrade(&confirmed(&request.operation)).expect("recover");
    assert_eq!(recovered.phase, UpgradePhase::Activated);
    assert_activated_equal_content_locations(&request, &database, &original);

    let (_directory, request, database, original) = interrupt_equal_content_at_retained_shm();
    let activated = activate_upgrade(&confirmed(&request.operation)).expect("activate");
    assert_eq!(activated.phase, UpgradePhase::Activated);
    assert_activated_equal_content_locations(&request, &database, &original);
}

fn interrupt_equal_content_at_retained_shm() -> (
    crate::test_support::TempHome,
    UpgradePrepareRequest,
    PathBuf,
    Vec<u8>,
) {
    let (directory, request, database) = current_import_fixture();
    prepare_upgrade(&request).expect("prepare");
    let original = fs::read(&database).expect("original");
    let candidate = fs::read(request.operation.join("candidate.db")).expect("candidate");
    assert_eq!(
        original, candidate,
        "each fixture independently keeps equal original and candidate bytes"
    );
    fs::write(sidecar(&database, "-shm"), b"leftover-shm").expect("leftover shm");
    inject_upgrade_fault(Some(UpgradeFault::RetainedShm));
    activate_upgrade(&confirmed(&request.operation)).expect_err("retained shm");
    inject_upgrade_fault(None);
    assert!(
        sidecar(&request.operation.join("retained-original.db"), "-shm").is_file(),
        "SHM retained before main"
    );
    assert!(
        !request.operation.join("retained-original.db").exists(),
        "retained main must be absent after RetainedShm"
    );
    assert_eq!(fs::read(&database).expect("live main"), original);
    (directory, request, database, original)
}

fn assert_activated_equal_content_locations(
    request: &UpgradePrepareRequest,
    database: &Path,
    original: &[u8],
) {
    let retained = request.operation.join("retained-original.db");
    let candidate = request.operation.join("candidate.db");
    assert!(
        retained.is_file(),
        "retained main location after completion"
    );
    assert_eq!(fs::read(&retained).expect("retained main"), original);
    assert_eq!(fs::read(database).expect("live main"), original);
    assert_eq!(fs::read(&candidate).expect("candidate"), original);
}

#[cfg(unix)]
fn alias_or_reparse(error: &MigrationError) -> bool {
    let text = error.to_string();
    text.contains("symlink") || text.contains("reparse") || text.contains("alias")
}

#[cfg(unix)]
#[test]
fn upgrade_review_v5_c1_c6_dangling_retained_wal_refuses_before_effects() {
    let (_directory, request, _) = sidecar_fixture();
    prepare_upgrade(&request).expect("prepare");
    let retained_wal = sidecar(&request.operation.join("retained-original.db"), "-wal");
    std::os::unix::fs::symlink("missing-retained-wal", &retained_wal).expect("dangling retained");
    refuse_mutating_without_effects(&request.operation);
    let metadata = fs::symlink_metadata(&retained_wal).expect("no-follow");
    assert!(metadata.file_type().is_symlink());
}

#[cfg(unix)]
#[test]
fn upgrade_review_v5_c1_c6_prepare_absent_sidecar_aliases_refuse_before_effects() {
    for suffix in ["-wal", "-journal"] {
        for live_link in [false, true] {
            let (directory, request, database) = fixture();
            let live = sidecar(&database, suffix);
            if live.exists() {
                fs::remove_file(&live).expect("clear present sidecar");
            }
            if live_link {
                let real = directory.path().join(format!("absent-target{suffix}"));
                fs::write(&real, b"not-a-recorded-sidecar").expect("link target");
                std::os::unix::fs::symlink(&real, &live).expect("live link");
            } else {
                std::os::unix::fs::symlink("missing-absent-sidecar", &live).expect("dangling");
            }
            let before = directory_snapshot(directory.path());
            let error = prepare_upgrade(&request).expect_err("prepare alias");
            assert!(
                alias_or_reparse(&error),
                "{suffix} live_link={live_link}: {error}"
            );
            assert_eq!(
                directory_snapshot(directory.path()),
                before,
                "{suffix} live_link={live_link}"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn upgrade_review_v5_c1_c6_live_wal_and_journal_aliases_refuse_before_effects() {
    for suffix in ["-wal", "-journal"] {
        let (directory, request, database) = sidecar_fixture();
        prepare_upgrade(&request).expect("prepare");
        let live = sidecar(&database, suffix);
        let original = fs::read(&live).expect("live sidecar");
        fs::remove_file(&live).expect("replace with alias");
        std::os::unix::fs::symlink("missing-live-sidecar", &live).expect("dangling live");
        let before = directory_snapshot(directory.path());
        let error = activate_upgrade(&confirmed(&request.operation)).expect_err("alias");
        assert!(alias_or_reparse(&error), "{suffix}: {error}");
        assert_eq!(directory_snapshot(directory.path()), before, "{suffix}");
        fs::remove_file(&live).expect("clear dangling");
        fs::write(&live, &original).expect("restore bytes");
        let real = directory.path().join(format!("real{suffix}"));
        fs::rename(&live, &real).expect("move real");
        std::os::unix::fs::symlink(&real, &live).expect("live symlink");
        let before = directory_snapshot(directory.path());
        let error = activate_upgrade(&confirmed(&request.operation)).expect_err("symlink");
        assert!(alias_or_reparse(&error), "{suffix} symlink: {error}");
        assert_eq!(
            directory_snapshot(directory.path()),
            before,
            "{suffix} symlink"
        );
    }
}

#[cfg(unix)]
#[test]
fn upgrade_review_v5_c1_c6_retained_main_symlink_refuses_before_effects() {
    let (_directory, request, database) = fixture();
    prepare_upgrade(&request).expect("prepare");
    let retained = request.operation.join("retained-original.db");
    std::os::unix::fs::symlink(&database, &retained).expect("retained main symlink");
    refuse_mutating_without_effects(&request.operation);
    let metadata = fs::symlink_metadata(&retained).expect("no-follow");
    assert!(metadata.file_type().is_symlink());
}

#[test]
fn upgrade_review_v5_c1_untracked_shm_directory_refuses_before_effects() {
    let (_directory, request, database) = fixture();
    prepare_upgrade(&request).expect("prepare");
    let shm = sidecar(&database, "-shm");
    if shm.exists() {
        fs::remove_file(&shm).expect("clear shm file");
    }
    fs::create_dir(&shm).expect("shm directory");
    refuse_mutating_without_effects(&request.operation);
    assert!(fs::symlink_metadata(&shm).expect("kept").is_dir());
}

#[test]
fn upgrade_review_v5_c6_prepare_shm_directory_refuses_before_effects() {
    let (directory, request, database) = fixture();
    let shm = sidecar(&database, "-shm");
    if shm.exists() {
        fs::remove_file(&shm).expect("clear shm file");
    }
    fs::create_dir(&shm).expect("shm directory");
    let before = directory_snapshot(directory.path());
    prepare_upgrade(&request).expect_err("shm directory");
    assert!(
        !request.operation.exists(),
        "prepare must refuse before creating the operation directory"
    );
    assert_eq!(directory_snapshot(directory.path()), before);
}

#[cfg(unix)]
#[test]
fn upgrade_review_v5_c6_prepare_shm_alias_refuses_before_effects() {
    for live_link in [false, true] {
        let (directory, request, database) = fixture();
        let shm = sidecar(&database, "-shm");
        if shm.exists() {
            fs::remove_file(&shm).expect("clear shm file");
        }
        if live_link {
            let real = directory.path().join("untracked-shm-target");
            fs::write(&real, b"not-hashed-shm").expect("link target");
            std::os::unix::fs::symlink(&real, &shm).expect("live shm link");
        } else {
            std::os::unix::fs::symlink("missing-shm", &shm).expect("dangling shm");
        }
        let before = directory_snapshot(directory.path());
        let error = prepare_upgrade(&request).expect_err("shm alias");
        assert!(alias_or_reparse(&error), "live_link={live_link}: {error}");
        assert!(
            !request.operation.exists(),
            "live_link={live_link}: prepare must refuse before creating the operation directory"
        );
        assert_eq!(
            directory_snapshot(directory.path()),
            before,
            "live_link={live_link}"
        );
    }
}

#[cfg(unix)]
#[test]
fn upgrade_review_v5_c6_rolling_back_absent_dangling_retained_wal_refuses_before_effects() {
    let (_directory, request, _) = fixture();
    prepare_upgrade(&request).expect("prepare");
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    assert!(
        load_journal_records(&request.operation)[0]
            .live_wal_sha256
            .is_none(),
        "fixture must record Absent WAL"
    );
    append_rolling_back(&request.operation);
    let retained_wal = sidecar(&request.operation.join("retained-original.db"), "-wal");
    std::os::unix::fs::symlink("missing-absent-retained-wal", &retained_wal).expect("dangling");
    refuse_mutating_without_effects(&request.operation);
    let metadata = fs::symlink_metadata(&retained_wal).expect("kept");
    assert!(metadata.file_type().is_symlink());
}

#[cfg(unix)]
#[test]
fn upgrade_review_v5_c6_rolling_back_absent_dangling_live_wal_refuses_before_effects() {
    let (_directory, request, database) = fixture();
    prepare_upgrade(&request).expect("prepare");
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    assert!(
        load_journal_records(&request.operation)[0]
            .live_wal_sha256
            .is_none(),
        "fixture must record Absent WAL"
    );
    append_rolling_back(&request.operation);
    let live_wal = sidecar(&database, "-wal");
    std::os::unix::fs::symlink("missing-absent-live-wal", &live_wal).expect("dangling");
    refuse_mutating_without_effects(&request.operation);
    let metadata = fs::symlink_metadata(&live_wal).expect("kept");
    assert!(metadata.file_type().is_symlink());
}

#[cfg(unix)]
#[test]
fn upgrade_review_v5_c6_candidate_symlink_equal_bytes_refuses_before_effects() {
    let (directory, request, _) = fixture();
    prepare_upgrade(&request).expect("prepare");
    let candidate = request.operation.join("candidate.db");
    let equal = directory.path().join("equal-candidate.db");
    fs::copy(&candidate, &equal).expect("equal bytes");
    fs::remove_file(&candidate).expect("replace candidate");
    std::os::unix::fs::symlink(&equal, &candidate).expect("candidate symlink");
    refuse_without_effects(&request.operation);
    let metadata = fs::symlink_metadata(&candidate).expect("kept");
    assert!(metadata.file_type().is_symlink());
}

#[cfg(unix)]
#[test]
fn upgrade_review_v5_c6_journal_record_symlink_equal_bytes_refuses_before_effects() {
    let (directory, request, _) = fixture();
    prepare_upgrade(&request).expect("prepare");
    let records = load_journal_records(&request.operation);
    assert_eq!(
        records[0].sequence, 1,
        "publisher writes initial sequence 1"
    );
    let record = request.operation.join("journal").join("00000001.json");
    assert!(record.is_file(), "initial record path is 00000001.json");
    let equal = directory.path().join("equal-journal-record.json");
    fs::copy(&record, &equal).expect("equal json");
    fs::remove_file(&record).expect("replace record");
    std::os::unix::fs::symlink(&equal, &record).expect("journal symlink");
    refuse_without_effects(&request.operation);
    let metadata = fs::symlink_metadata(&record).expect("kept");
    assert!(metadata.file_type().is_symlink());
}

#[cfg(unix)]
#[test]
fn upgrade_review_v5_c6_journal_dir_symlink_refuses_before_effects() {
    let (_directory, request, _) = fixture();
    prepare_upgrade(&request).expect("prepare");
    let journal = request.operation.join("journal");
    let real = request.operation.join("journal-real");
    fs::rename(&journal, &real).expect("move journal dir");
    std::os::unix::fs::symlink(&real, &journal).expect("journal dir symlink");
    refuse_without_effects(&request.operation);
    let metadata = fs::symlink_metadata(&journal).expect("kept");
    assert!(metadata.file_type().is_symlink());
}

#[test]
fn upgrade_review_v5_c9_activated_original_live_with_retained_refuses_before_effects() {
    let (_directory, request, database) = fixture();
    let original = fs::read(&database).expect("original");
    prepare_upgrade(&request).expect("prepare");
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    let candidate = fs::read(&database).expect("candidate live");
    assert_ne!(original, candidate);
    let retained = request.operation.join("retained-original.db");
    assert_eq!(fs::read(&retained).expect("retained"), original);
    fs::write(&database, &original).expect("copy original live");
    assert!(retained.is_file(), "retained stays intact");
    refuse_without_effects(&request.operation);
    assert_eq!(fs::read(&database).expect("kept original live"), original);
    assert_eq!(fs::read(&retained).expect("kept retained"), original);
}

fn interrupt_retained_main_dual_shm_legal_temp() -> (
    crate::test_support::TempHome,
    UpgradePrepareRequest,
    PathBuf,
) {
    let (directory, request, database) = sidecar_fixture();
    prepare_upgrade(&request).expect("prepare");
    inject_upgrade_fault(Some(UpgradeFault::RetainedMain));
    activate_upgrade(&confirmed(&request.operation)).expect_err("retained main");
    inject_upgrade_fault(None);
    assert!(!database.exists(), "live main missing after RetainedMain");
    let retained = request.operation.join("retained-original.db");
    assert!(retained.is_file(), "retained main present");
    let retained_shm = sidecar(&retained, "-shm");
    assert!(retained_shm.is_file(), "retained SHM already moved");
    fs::write(sidecar(&database, "-shm"), b"live-shm-after-retain").expect("live shm");
    plant_legal_journal_temp(&request.operation, 3, JournalKind::Activated);
    (directory, request, database)
}

fn refuse_activate_or_recover_without_effects(operation: &Path, recover: bool) {
    let parent = operation.parent().expect("parent");
    let before = directory_snapshot(parent);
    let temp = operation.join("journal-publish.tmp");
    assert!(temp.is_file(), "legal journal temp planted");
    if recover {
        recover_upgrade(&confirmed(operation)).expect_err("recover");
        assert!(temp.is_file(), "legal journal temp kept after recover");
        assert_eq!(directory_snapshot(parent), before, "after recover");
    } else {
        activate_upgrade(&confirmed(operation)).expect_err("activate");
        assert!(temp.is_file(), "legal journal temp kept after activate");
        assert_eq!(directory_snapshot(parent), before, "after activate");
    }
}

#[test]
fn upgrade_review_v5_c1_c2_retained_main_dual_shm_legal_temp_recover_refuses_before_effects() {
    let (_directory, request, _) = interrupt_retained_main_dual_shm_legal_temp();
    refuse_activate_or_recover_without_effects(&request.operation, true);
}

#[test]
fn upgrade_review_v5_c1_c2_retained_main_dual_shm_legal_temp_activate_refuses_before_effects() {
    let (_directory, request, _) = interrupt_retained_main_dual_shm_legal_temp();
    refuse_activate_or_recover_without_effects(&request.operation, false);
}

fn interrupt_retained_main_live_only_shm_legal_temp() -> (
    crate::test_support::TempHome,
    UpgradePrepareRequest,
    PathBuf,
) {
    let (directory, request, database) = fixture();
    prepare_upgrade(&request).expect("prepare");
    inject_upgrade_fault(Some(UpgradeFault::RetainedMain));
    activate_upgrade(&confirmed(&request.operation)).expect_err("retained main");
    inject_upgrade_fault(None);
    assert!(!database.exists(), "live main missing after RetainedMain");
    let retained = request.operation.join("retained-original.db");
    assert!(retained.is_file(), "retained main present");
    assert!(
        !sidecar(&retained, "-shm").exists(),
        "retained SHM must be absent for live-only"
    );
    for suffix in ["-wal", "-journal", "-shm"] {
        assert!(
            !sidecar(&database, suffix).exists(),
            "live {suffix} must start absent"
        );
    }
    fs::write(sidecar(&database, "-shm"), b"live-only-shm-after-retain").expect("live shm");
    plant_legal_journal_temp(&request.operation, 3, JournalKind::Activated);
    (directory, request, database)
}

#[test]
fn upgrade_review_v5_c1_c2_retained_main_live_only_shm_legal_temp_recover_refuses_before_effects() {
    let (_directory, request, _) = interrupt_retained_main_live_only_shm_legal_temp();
    refuse_activate_or_recover_without_effects(&request.operation, true);
}

#[test]
fn upgrade_review_v5_c1_c2_retained_main_live_only_shm_legal_temp_activate_refuses_before_effects()
{
    let (_directory, request, _) = interrupt_retained_main_live_only_shm_legal_temp();
    refuse_activate_or_recover_without_effects(&request.operation, false);
}

fn interrupt_equal_content_at_empty_sidecar(
    suffix: &str,
    fault: UpgradeFault,
) -> (
    crate::test_support::TempHome,
    UpgradePrepareRequest,
    PathBuf,
    Vec<u8>,
) {
    let (directory, request, database) = current_import_fixture();
    fs::write(sidecar(&database, suffix), b"").expect("empty durable sidecar");
    prepare_upgrade(&request).expect("prepare");
    let original = fs::read(&database).expect("original");
    let candidate = fs::read(request.operation.join("candidate.db")).expect("candidate");
    assert_eq!(
        original, candidate,
        "each fixture independently keeps equal original and candidate bytes"
    );
    let prepared = &load_journal_records(&request.operation)[0];
    match suffix {
        "-wal" => assert_eq!(prepared.live_wal_sha256.as_deref().map(str::len), Some(64)),
        "-journal" => assert_eq!(
            prepared.live_journal_sha256.as_deref().map(str::len),
            Some(64)
        ),
        _ => panic!("durable sidecar"),
    }
    inject_upgrade_fault(Some(fault));
    activate_upgrade(&confirmed(&request.operation)).expect_err("retain sidecar");
    inject_upgrade_fault(None);
    assert!(
        sidecar(&request.operation.join("retained-original.db"), suffix).is_file(),
        "{suffix} retained before main"
    );
    assert_eq!(
        fs::read(sidecar(
            &request.operation.join("retained-original.db"),
            suffix
        ))
        .expect("empty"),
        b"",
        "{suffix} retained empty bytes"
    );
    assert!(!sidecar(&database, suffix).exists(), "live {suffix} moved");
    assert!(
        !request.operation.join("retained-original.db").exists(),
        "retained main must be absent after {suffix} retain"
    );
    assert_eq!(fs::read(&database).expect("live main"), original);
    (directory, request, database, original)
}

fn assert_activated_equal_content_empty_sidecar(
    request: &UpgradePrepareRequest,
    database: &Path,
    original: &[u8],
    suffix: &str,
) {
    assert_activated_equal_content_locations(request, database, original);
    let retained = sidecar(&request.operation.join("retained-original.db"), suffix);
    assert!(retained.is_file(), "retained {suffix} location");
    assert_eq!(fs::read(&retained).expect("retained empty"), b"");
    assert!(
        !sidecar(database, suffix).exists(),
        "live {suffix} stays absent after completion"
    );
}

#[test]
fn upgrade_review_v5_t3_c5_retained_empty_wal_recover_completes_by_location() {
    let (_directory, request, database, original) =
        interrupt_equal_content_at_empty_sidecar("-wal", UpgradeFault::RetainedWal);
    let recovered = recover_upgrade(&confirmed(&request.operation)).expect("recover");
    assert_eq!(recovered.phase, UpgradePhase::Activated);
    assert_activated_equal_content_empty_sidecar(&request, &database, &original, "-wal");
}

#[test]
fn upgrade_review_v5_t3_c5_retained_empty_wal_activate_completes_by_location() {
    let (_directory, request, database, original) =
        interrupt_equal_content_at_empty_sidecar("-wal", UpgradeFault::RetainedWal);
    let activated = activate_upgrade(&confirmed(&request.operation)).expect("activate");
    assert_eq!(activated.phase, UpgradePhase::Activated);
    assert_activated_equal_content_empty_sidecar(&request, &database, &original, "-wal");
}

#[test]
fn upgrade_review_v5_t3_c5_retained_empty_journal_recover_completes_by_location() {
    let (_directory, request, database, original) =
        interrupt_equal_content_at_empty_sidecar("-journal", UpgradeFault::RetainedJournal);
    let recovered = recover_upgrade(&confirmed(&request.operation)).expect("recover");
    assert_eq!(recovered.phase, UpgradePhase::Activated);
    assert_activated_equal_content_empty_sidecar(&request, &database, &original, "-journal");
}

#[test]
fn upgrade_review_v5_t3_c5_retained_empty_journal_activate_completes_by_location() {
    let (_directory, request, database, original) =
        interrupt_equal_content_at_empty_sidecar("-journal", UpgradeFault::RetainedJournal);
    let activated = activate_upgrade(&confirmed(&request.operation)).expect("activate");
    assert_eq!(activated.phase, UpgradePhase::Activated);
    assert_activated_equal_content_empty_sidecar(&request, &database, &original, "-journal");
}

fn refuse_status_without_effects(operation: &Path) {
    let parent = operation.parent().expect("parent");
    let before = directory_snapshot(parent);
    upgrade_status(operation).expect_err("status");
    assert_eq!(directory_snapshot(parent), before, "after status");
}

fn refuse_rollback_without_effects(operation: &Path) {
    let parent = operation.parent().expect("parent");
    let before = directory_snapshot(parent);
    let temp = operation.join("journal-publish.tmp");
    let had_temp = temp.is_file();
    rollback_upgrade(&confirmed(operation)).expect_err("rollback");
    if had_temp {
        assert!(temp.is_file(), "legal journal temp kept after rollback");
    }
    assert_eq!(directory_snapshot(parent), before, "after rollback");
}

fn interrupt_retained_main_only() -> (
    crate::test_support::TempHome,
    UpgradePrepareRequest,
    PathBuf,
) {
    let (directory, request, database) = fixture();
    prepare_upgrade(&request).expect("prepare");
    inject_upgrade_fault(Some(UpgradeFault::RetainedMain));
    activate_upgrade(&confirmed(&request.operation)).expect_err("retained main");
    inject_upgrade_fault(None);
    assert!(!database.exists(), "live main missing after RetainedMain");
    assert!(
        request.operation.join("retained-original.db").is_file(),
        "retained main present"
    );
    (directory, request, database)
}

fn assert_temps_absent(operation: &Path) {
    assert!(
        !operation.join("journal-publish.tmp").exists(),
        "journal temp absent"
    );
    assert!(
        !operation.join("publish-staging").exists(),
        "complete staging absent"
    );
    assert!(
        !operation.join("publish-staging.partial").exists(),
        "partial staging absent"
    );
}

fn assert_rolled_back_empty_sidecar(
    request: &UpgradePrepareRequest,
    database: &Path,
    original: &[u8],
    suffix: &str,
) {
    assert_eq!(fs::read(database).expect("restored original"), original);
    let live = sidecar(database, suffix);
    assert!(live.is_file(), "empty {suffix} restored live");
    assert_eq!(fs::read(&live).expect("empty live"), b"");
    assert!(
        !request.operation.join("retained-original.db").exists(),
        "retained main stays absent"
    );
    assert!(
        !sidecar(&request.operation.join("retained-original.db"), suffix).exists(),
        "retained {suffix} restored away"
    );
    let kinds: Vec<_> = load_journal_records(&request.operation)
        .into_iter()
        .map(|record| (record.sequence, record.kind))
        .collect();
    assert_eq!(
        kinds,
        vec![
            (1, JournalKind::Prepared),
            (2, JournalKind::Activating),
            (3, JournalKind::RollingBack),
            (4, JournalKind::RolledBack),
        ]
    );
    assert_temps_absent(&request.operation);
}

fn rolling_back_after_activate(request: &UpgradePrepareRequest, database: &Path) {
    prepare_upgrade(request).expect("prepare");
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    assert_ne!(
        fs::read(database).expect("candidate live"),
        fs::read(request.operation.join("retained-original.db")).expect("retained original"),
        "fixture must keep distinct original and candidate"
    );
    append_rolling_back(&request.operation);
}

#[test]
fn upgrade_review_v7_status_rolling_back_missing_original_refuses_before_effects() {
    let (_directory, request, _) = fixture();
    rolling_back_after_activate(&request, &request.database);
    fs::remove_file(request.operation.join("retained-original.db")).expect("drop original");
    refuse_status_without_effects(&request.operation);
}

#[test]
fn upgrade_review_v7_status_rolling_back_corrupt_retained_refuses_before_effects() {
    let (_directory, request, _) = fixture();
    rolling_back_after_activate(&request, &request.database);
    let retained = request.operation.join("retained-original.db");
    fs::write(&retained, b"corrupt-retained-main").expect("corrupt");
    refuse_status_without_effects(&request.operation);
    assert_eq!(fs::read(&retained).expect("kept"), b"corrupt-retained-main");
}

#[cfg(unix)]
#[test]
fn upgrade_review_v7_status_rolling_back_retained_alias_refuses_before_effects() {
    let (_directory, request, _) = fixture();
    rolling_back_after_activate(&request, &request.database);
    let retained = request.operation.join("retained-original.db");
    let real = request.operation.join("retained-real.db");
    fs::rename(&retained, &real).expect("move retained");
    std::os::unix::fs::symlink(&real, &retained).expect("retained alias");
    refuse_status_without_effects(&request.operation);
    let metadata = fs::symlink_metadata(&retained).expect("kept");
    assert!(metadata.file_type().is_symlink());
}

#[test]
fn upgrade_review_v7_status_activating_missing_main_dual_shm_refuses_before_effects() {
    let (_directory, request, _) = interrupt_retained_main_dual_shm_legal_temp();
    refuse_status_without_effects(&request.operation);
}

#[test]
fn upgrade_review_v7_status_activating_missing_main_live_only_shm_refuses_before_effects() {
    let (_directory, request, _) = interrupt_retained_main_live_only_shm_legal_temp();
    refuse_status_without_effects(&request.operation);
}

#[test]
fn upgrade_review_v7_status_activating_duplicate_original_refuses_before_effects() {
    let (_directory, request, database) = interrupt_retained_main_only();
    fs::copy(request.operation.join("retained-original.db"), &database)
        .expect("duplicate original live");
    refuse_status_without_effects(&request.operation);
    assert!(database.is_file(), "duplicate original kept");
    assert!(request.operation.join("retained-original.db").is_file());
}

#[test]
fn upgrade_review_v7_rollback_activating_missing_main_dual_shm_refuses_before_effects() {
    let (_directory, request, _) = interrupt_retained_main_dual_shm_legal_temp();
    refuse_rollback_without_effects(&request.operation);
}

#[test]
fn upgrade_review_v7_rollback_activating_missing_main_live_only_shm_refuses_before_effects() {
    let (_directory, request, _) = interrupt_retained_main_live_only_shm_legal_temp();
    refuse_rollback_without_effects(&request.operation);
}

#[test]
fn upgrade_review_v7_t3_c5_retained_empty_wal_rollback_completes_by_location() {
    let (_directory, request, database, original) =
        interrupt_equal_content_at_empty_sidecar("-wal", UpgradeFault::RetainedWal);
    let rolled = rollback_upgrade(&confirmed(&request.operation)).expect("rollback");
    assert_eq!(rolled.phase, UpgradePhase::RolledBack);
    assert_rolled_back_empty_sidecar(&request, &database, &original, "-wal");
}

#[test]
fn upgrade_review_v7_t3_c5_retained_empty_journal_rollback_completes_by_location() {
    let (_directory, request, database, original) =
        interrupt_equal_content_at_empty_sidecar("-journal", UpgradeFault::RetainedJournal);
    let rolled = rollback_upgrade(&confirmed(&request.operation)).expect("rollback");
    assert_eq!(rolled.phase, UpgradePhase::RolledBack);
    assert_rolled_back_empty_sidecar(&request, &database, &original, "-journal");
}

#[test]
fn upgrade_review_v7_status_legal_split_retain_reports_activating() {
    let (_directory, request, database, original) =
        interrupt_equal_content_at_empty_sidecar("-wal", UpgradeFault::RetainedWal);
    let parent = request.operation.parent().expect("parent");
    let before = directory_snapshot(parent);
    let status = upgrade_status(&request.operation).expect("legal split status");
    assert_eq!(status.phase, UpgradePhase::Activating);
    assert_eq!(directory_snapshot(parent), before, "status is observation");
    assert_eq!(fs::read(&database).expect("untouched"), original);
}

#[test]
fn upgrade_review_v7_status_equal_content_activated_reports_activated() {
    let (_directory, request, database) = current_import_fixture();
    let original = fs::read(&database).expect("original");
    prepare_upgrade(&request).expect("prepare");
    assert_eq!(
        original,
        fs::read(request.operation.join("candidate.db")).expect("candidate")
    );
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    let parent = request.operation.parent().expect("parent");
    let before = directory_snapshot(parent);
    let status = upgrade_status(&request.operation).expect("equal content status");
    assert_eq!(status.phase, UpgradePhase::Activated);
    assert_eq!(directory_snapshot(parent), before, "status is observation");
}

#[test]
fn upgrade_review_v7_status_rolled_back_resumed_stays_terminal() {
    let (_directory, request, database) = fixture();
    prepare_upgrade(&request).expect("prepare");
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    rollback_upgrade(&confirmed(&request.operation)).expect("rollback");
    let mut bytes = fs::read(&database).expect("restored");
    bytes.push(b'X');
    fs::write(&database, &bytes).expect("resume");
    let parent = request.operation.parent().expect("parent");
    let before = directory_snapshot(parent);
    let status = upgrade_status(&request.operation).expect("terminal status");
    assert_eq!(status.phase, UpgradePhase::RolledBack);
    assert_eq!(directory_snapshot(parent), before, "status is observation");
    assert_eq!(fs::read(&database).expect("left resumed"), bytes);
}

#[test]
fn upgrade_review_v7_status_rolling_back_both_mains_missing_refuses_before_effects() {
    let (_directory, request, database) = fixture();
    rolling_back_after_activate(&request, &database);
    fs::remove_file(&database).expect("drop live");
    fs::remove_file(request.operation.join("retained-original.db")).expect("drop retained");
    refuse_status_without_effects(&request.operation);
}

#[test]
fn upgrade_review_v7_status_rolling_back_legal_journal_temp_reports_rolling_back() {
    let (_directory, request, _) = fixture();
    rolling_back_after_activate(&request, &request.database);
    let temp = plant_legal_journal_temp(&request.operation, 5, JournalKind::RolledBack);
    let parent = request.operation.parent().expect("parent");
    let before = directory_snapshot(parent);
    let status = upgrade_status(&request.operation).expect("legal temp status");
    assert_eq!(status.phase, UpgradePhase::RollingBack);
    assert_eq!(directory_snapshot(parent), before, "status is observation");
    assert!(temp.is_file(), "legal journal temp kept");
}

#[test]
fn upgrade_review_v7_status_activated_legal_journal_temp_reports_activated() {
    let (_directory, request, _) = fixture();
    prepare_upgrade(&request).expect("prepare");
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    let temp = plant_legal_journal_temp(&request.operation, 4, JournalKind::Finalized);
    let parent = request.operation.parent().expect("parent");
    let before = directory_snapshot(parent);
    let status = upgrade_status(&request.operation).expect("legal temp status");
    assert_eq!(status.phase, UpgradePhase::Activated);
    assert_eq!(directory_snapshot(parent), before, "status is observation");
    assert!(temp.is_file(), "legal journal temp kept");
}

#[test]
fn upgrade_review_v7_status_rolling_back_foreign_partial_refuses_before_effects() {
    let (_directory, request, _) = fixture();
    rolling_back_after_activate(&request, &request.database);
    let partial = plant_foreign_partial(&request.operation);
    refuse_status_without_effects(&request.operation);
    assert_eq!(
        fs::read(&partial).expect("kept"),
        b"foreign-staging-partial"
    );
}

#[test]
fn upgrade_review_v7_status_rolling_back_foreign_staging_refuses_before_effects() {
    let (_directory, request, _) = fixture();
    rolling_back_after_activate(&request, &request.database);
    let staged = publish_staging(&request.operation);
    fs::write(&staged, b"foreign-full-staging").expect("foreign staging");
    refuse_status_without_effects(&request.operation);
    assert_eq!(fs::read(&staged).expect("kept"), b"foreign-full-staging");
}

#[test]
fn upgrade_review_v7_status_rolling_back_foreign_journal_temp_refuses_before_effects() {
    let (_directory, request, _) = fixture();
    rolling_back_after_activate(&request, &request.database);
    let temp = request.operation.join("journal-publish.tmp");
    fs::write(&temp, b"foreign-journal-temp").expect("foreign journal temp");
    refuse_status_without_effects(&request.operation);
    assert_eq!(fs::read(&temp).expect("kept"), b"foreign-journal-temp");
}

#[test]
fn upgrade_review_v7_status_rolling_back_restored_wal_before_main_reports_rolling_back() {
    let (_directory, request, database) = sidecar_fixture();
    let original_wal = fs::read(sidecar(&database, "-wal")).expect("original wal");
    prepare_upgrade(&request).expect("prepare");
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    inject_upgrade_fault(Some(UpgradeFault::RestoredWal));
    rollback_upgrade(&confirmed(&request.operation)).expect_err("restored wal");
    inject_upgrade_fault(None);
    assert_eq!(
        fs::read(sidecar(&database, "-wal")).expect("wal restored live"),
        original_wal
    );
    assert!(
        request.operation.join("retained-original.db").is_file(),
        "main still retained"
    );
    let parent = request.operation.parent().expect("parent");
    let before = directory_snapshot(parent);
    let status = upgrade_status(&request.operation).expect("legal restored-sidecar status");
    assert_eq!(status.phase, UpgradePhase::RollingBack);
    assert_eq!(directory_snapshot(parent), before, "status is observation");
}

fn plant_foreign_staging_bytes(operation: &Path) -> PathBuf {
    let staged = publish_staging(operation);
    fs::write(&staged, b"foreign-full-staging").expect("foreign staging");
    staged
}

fn plant_foreign_journal_temp_bytes(operation: &Path) -> PathBuf {
    let temp = operation.join("journal-publish.tmp");
    fs::write(&temp, b"foreign-journal-temp").expect("foreign journal temp");
    temp
}

fn prepared_foreign_temp_fixture() -> (crate::test_support::TempHome, UpgradePrepareRequest) {
    let (directory, request, _) = fixture();
    prepare_upgrade(&request).expect("prepare");
    (directory, request)
}

fn intent_only_prepared_foreign_temp_fixture()
-> (crate::test_support::TempHome, UpgradePrepareRequest) {
    let (directory, request, _) = fixture();
    prepare_upgrade(&request).expect("prepare");
    inject_upgrade_fault(Some(UpgradeFault::ActivatingRecorded));
    activate_upgrade(&confirmed(&request.operation)).expect_err("activating intent");
    inject_upgrade_fault(None);
    (directory, request)
}

fn activating_split_retain_foreign_temp_fixture()
-> (crate::test_support::TempHome, UpgradePrepareRequest) {
    let (directory, request, database, original) =
        interrupt_equal_content_at_empty_sidecar("-wal", UpgradeFault::RetainedWal);
    let journal: Vec<_> = load_journal_records(&request.operation)
        .into_iter()
        .map(|record| (record.sequence, record.kind))
        .collect();
    assert_eq!(
        journal,
        vec![(1, JournalKind::Prepared), (2, JournalKind::Activating),]
    );
    assert_eq!(fs::read(&database).expect("live original"), original);
    assert!(!sidecar(&database, "-wal").exists(), "live WAL retained");
    assert_eq!(
        fs::read(sidecar(
            &request.operation.join("retained-original.db"),
            "-wal"
        ))
        .expect("retained WAL"),
        b""
    );
    assert!(
        !request.operation.join("retained-original.db").exists(),
        "retained main not reached"
    );
    (directory, request)
}

fn activated_foreign_temp_fixture() -> (crate::test_support::TempHome, UpgradePrepareRequest) {
    let (directory, request, _) = fixture();
    prepare_upgrade(&request).expect("prepare");
    activate_upgrade(&confirmed(&request.operation)).expect("activate");
    (directory, request)
}

type ForeignTempSetup = fn() -> (crate::test_support::TempHome, UpgradePrepareRequest);
type ForeignTempPlant = fn(&Path) -> PathBuf;
type ForeignTempRow = (&'static str, ForeignTempPlant, &'static [u8]);

#[test]
fn upgrade_review_v7_status_nonterminal_foreign_temps_refuse_before_effects() {
    let setups: &[(&str, ForeignTempSetup)] = &[
        ("prepared", prepared_foreign_temp_fixture),
        (
            "intent-only-prepared",
            intent_only_prepared_foreign_temp_fixture,
        ),
        (
            "activating-split-retain",
            activating_split_retain_foreign_temp_fixture,
        ),
        ("activated", activated_foreign_temp_fixture),
    ];
    let plants: &[ForeignTempRow] = &[
        ("partial", plant_foreign_partial, b"foreign-staging-partial"),
        (
            "staging",
            plant_foreign_staging_bytes,
            b"foreign-full-staging",
        ),
        (
            "journal-temp",
            plant_foreign_journal_temp_bytes,
            b"foreign-journal-temp",
        ),
    ];
    let mut violations = Vec::new();
    for (phase_label, setup) in setups {
        for (temp_label, plant, kept) in plants {
            let (_directory, request) = setup();
            let path = plant(&request.operation);
            let parent = request.operation.parent().expect("parent");
            let before = directory_snapshot(parent);
            let status = upgrade_status(&request.operation);
            let after = directory_snapshot(parent);
            if after != before {
                violations.push(format!(
                    "{phase_label}/{temp_label}: status mutated inventory"
                ));
            }
            match status {
                Err(_) => {}
                Ok(report) => violations.push(format!(
                    "{phase_label}/{temp_label}: status succeeded as {:?}",
                    report.phase
                )),
            }
            match fs::read(&path) {
                Ok(bytes) if bytes == *kept => {}
                Ok(bytes) => violations.push(format!(
                    "{phase_label}/{temp_label}: kept {bytes:?}, want {kept:?}"
                )),
                Err(error) => violations.push(format!(
                    "{phase_label}/{temp_label}: lost foreign temp: {error}"
                )),
            }
        }
    }
    assert!(
        violations.is_empty(),
        "foreign-temp status matrix:\n{}",
        violations.join("\n")
    );
}
