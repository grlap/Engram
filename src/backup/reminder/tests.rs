use std::fs;

use chrono::{Duration, TimeZone};

use super::*;
use crate::{
    ObjectId, WorkGraphSnapshotCut,
    backup::{
        CaptureManifest,
        record::{
            Acknowledgement, BackupReceipt, Encoding, LastAttempt, OffHost, STORED_FORMAT_VERSION,
            StoredManifest,
        },
        target::{
            AdapterKind, PushLock, RECORD_FORMAT_VERSION, Statement, TargetConfig, TargetRequest,
            TargetState, set_target, write_state,
        },
    },
    test_support::temp_home,
};

/// Hours after a fixed instant.
fn at(hours: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0)
        .single()
        .expect("fixed test timestamp")
        + Duration::hours(hours)
}

fn project() -> ProjectId {
    ProjectId("reminder-project".into())
}

fn schema() -> ObjectId {
    ObjectId::from_canonical_bytes(b"store schema")
}

fn formats() -> AcceptedFormats {
    AcceptedFormats {
        store: Some(schema()),
    }
}

fn config() -> TargetConfig {
    TargetConfig {
        format_version: RECORD_FORMAT_VERSION,
        project: project().0,
        kind: CopyKind::Store,
        adapter: AdapterKind::Directory,
        dir: if cfg!(windows) {
            r"D:\copies".into()
        } else {
            "/srv/copies".into()
        },
        window_hours: 24,
        keep: 3,
        disclosure_authorized: Statement {
            by: "greg".into(),
            at: at(0),
        },
        off_host_asserted: Some(Statement {
            by: "greg".into(),
            at: at(0),
        }),
    }
}

fn receipt(identity: &ObjectId) -> BackupReceipt {
    receipt_for(identity, &project(), schema(), at(9), at(10))
}

/// A receipt for a store copy of `project` in `format`, captured and
/// received at the given times.
pub(crate) fn receipt_for(
    identity: &ObjectId,
    project: &ProjectId,
    format: ObjectId,
    captured: DateTime<Utc>,
    received: DateTime<Utc>,
) -> BackupReceipt {
    BackupReceipt {
        sha256: "a".repeat(64),
        target_identity: identity.clone(),
        at: received,
        acknowledgement: Acknowledgement::ReadBack,
        off_host: OffHost::Asserted,
        manifest: StoredManifest {
            format_version: STORED_FORMAT_VERSION,
            copy: "20261001T090000Z-copy".into(),
            target_identity: identity.clone(),
            encoding: Encoding::Gzip,
            stored_bytes: 100,
            capture: CaptureManifest {
                project_digest: crate::project_digest(project),
                kind: CopyKind::Store,
                cut: WorkGraphSnapshotCut {
                    work_feed: 7,
                    project_memory: 3,
                },
                capture_started_at: captured,
                bytes: 4096,
                sha256: "a".repeat(64),
                format_identity: format,
                build_fingerprint: None,
                source_revision: Some("unavailable".into()),
                host_name: None,
            },
        },
    }
}

/// Configures a directory target for `project` under `home` and, with
/// `copy`, records a store copy in the running build's format, captured and
/// confirmed a minute ago, so it qualifies now, with `last` as the last
/// attempt.
pub(crate) fn record_target(
    home: &Path,
    project: &ProjectId,
    copy: bool,
    last: Option<LastAttempt>,
) {
    let copies = home.join("copies");
    fs::create_dir_all(&copies).unwrap();
    let now = Utc::now();
    let configured = set_target(
        home,
        project,
        &TargetRequest {
            kind: CopyKind::Store,
            adapter: AdapterKind::Directory,
            dir: copies,
            disclosure_authorized_by: "greg".into(),
            off_host_asserted_by: Some("greg".into()),
            window_hours: 24,
            keep: 3,
        },
        now - Duration::minutes(2),
    )
    .unwrap();
    if !copy && last.is_none() {
        return;
    }
    let identity = configured.identity;
    let paths = RecordPaths::new(home, project, CopyKind::Store);
    let lock = PushLock::try_acquire(&paths).unwrap();
    let mut state = TargetState::empty(identity.clone());
    if copy {
        let format = crate::storage::running_schema_reference().unwrap();
        let captured = now - Duration::minutes(1);
        state.record_receipt(
            receipt_for(&identity, project, format, captured, captured),
            captured,
        );
    }
    state.last_attempt = last;
    write_state(&paths, &lock, &state).unwrap();
}

/// A last attempt that ended a moment ago.
pub(crate) fn recent_attempt(outcome: AttemptOutcome, code: Option<&str>) -> LastAttempt {
    let now = Utc::now();
    LastAttempt {
        started_at: now - Duration::seconds(30),
        ended_at: now - Duration::seconds(20),
        outcome,
        code: code.map(str::to_owned),
        message: code.map(|_| "it failed".to_owned()),
    }
}

fn attempt(outcome: AttemptOutcome, code: Option<&str>) -> LastAttempt {
    LastAttempt {
        started_at: at(11),
        ended_at: at(12),
        outcome,
        code: code.map(str::to_owned),
        message: code.map(|_| "it failed".to_owned()),
    }
}

/// A store copy captured at hour 9 and confirmed at hour 10 for a 24-hour
/// window, so it qualifies until hour 33, with `last` as the last attempt.
fn store(last: Option<LastAttempt>) -> Vec<(CopyKind, KindRecords)> {
    let config = config();
    let identity = config.identity().unwrap();
    let mut state = TargetState::empty(identity.clone());
    state.record_receipt(receipt(&identity), at(9));
    state.last_attempt = last;
    vec![(
        CopyKind::Store,
        KindRecords::Configured {
            config: Box::new(config),
            identity,
            state: Box::new(state),
        },
    )]
}

fn line(records: &[(CopyKind, KindRecords)], hours: i64) -> Option<String> {
    reminder_line(records, &formats(), at(hours))
}

#[test]
fn no_target_configured_gives_no_line() {
    assert_eq!(
        line(&[(CopyKind::Store, KindRecords::NotConfigured)], 20),
        None
    );
    assert_eq!(line(&[], 20), None);
}

#[test]
fn a_qualifying_copy_whose_last_push_succeeded_gives_no_line() {
    let uploaded = store(Some(attempt(AttemptOutcome::Uploaded, None)));
    assert_eq!(line(&uploaded, 20), None);
    let unchanged = store(Some(attempt(AttemptOutcome::Unchanged, None)));
    assert_eq!(line(&unchanged, 20), None);
}

#[test]
fn a_configured_target_in_local_mode_gives_one_line_with_the_reason() {
    // Past the window with no new push.
    let expired = line(&store(Some(attempt(AttemptOutcome::Uploaded, None))), 40).unwrap();
    assert_eq!(
        expired,
        "backup: mode local (store: backup_confirmation_expired); see engram backup status"
    );
    // A target with nothing confirmed yet.
    let config = config();
    let identity = config.identity().unwrap();
    let never = [(
        CopyKind::Store,
        KindRecords::Configured {
            config: Box::new(config),
            state: Box::new(TargetState::empty(identity.clone())),
            identity,
        },
    )];
    assert_eq!(
        line(&never, 20).unwrap(),
        "backup: mode local (store: backup_never_confirmed); see engram backup status"
    );
    // A record this build cannot use counts as configured.
    let unreadable = [(
        CopyKind::Store,
        KindRecords::Unreadable {
            path: "store.target.json".into(),
            reason: "damaged".into(),
        },
    )];
    assert_eq!(
        line(&unreadable, 20).unwrap(),
        "backup: mode local (store: backup_record_unreadable); see engram backup status"
    );
}

#[test]
fn a_failed_last_push_shows_while_an_earlier_copy_still_qualifies() {
    let failed = store(Some(attempt(
        AttemptOutcome::Failed,
        Some("backup_target_unreachable"),
    )));
    assert_eq!(
        line(&failed, 20).unwrap(),
        "backup: the last store push failed: backup_target_unreachable; an earlier copy still qualifies (store copy: off-host asserted; not verified); see engram backup status"
    );
    // Once the copy no longer qualifies, the one line names both.
    assert_eq!(
        line(&failed, 40).unwrap(),
        "backup: mode local (store: backup_confirmation_expired); the last store push failed: backup_target_unreachable; see engram backup status"
    );
}

#[test]
fn a_recorded_failure_code_is_printed_only_when_it_is_a_plain_code() {
    let printed =
        |code: Option<&str>| line(&store(Some(attempt(AttemptOutcome::Failed, code))), 20).unwrap();
    assert!(printed(None).contains("failed: no code recorded;"));
    for odd in [
        "",
        "Backup_Odd",
        "backup code",
        "backup_\u{1b}[31m",
        &"a".repeat(MAX_PRINTED_CODE_BYTES + 1),
    ] {
        let line = printed(Some(odd));
        assert!(
            line.contains("failed: unrecognised code;"),
            "{odd:?}: {line}"
        );
    }
    let longest = "a".repeat(MAX_PRINTED_CODE_BYTES);
    assert!(printed(Some(&longest)).contains(&longest));
}

#[test]
fn without_a_configuration_file_nothing_is_read_and_no_line_is_given() {
    let home = temp_home().unwrap();
    assert_eq!(backup_reminder(home.path(), &project()), None);
    assert!(!home.path().join("backup-records").exists());
    // A state file alone, even a damaged one, configures nothing.
    let paths = RecordPaths::new(home.path(), &project(), CopyKind::Store);
    fs::create_dir_all(&paths.directory).unwrap();
    fs::write(&paths.state, b"{ damaged").unwrap();
    assert_eq!(backup_reminder(home.path(), &project()), None);
}

#[test]
fn a_configured_target_on_disk_is_read_and_reminded() {
    let home = temp_home().unwrap();
    fs::create_dir_all(home.path().join("copies")).unwrap();
    set_target(
        home.path(),
        &project(),
        &TargetRequest {
            kind: CopyKind::Store,
            adapter: AdapterKind::Directory,
            dir: home.path().join("copies"),
            disclosure_authorized_by: "greg".into(),
            off_host_asserted_by: Some("greg".into()),
            window_hours: 24,
            keep: 3,
        },
        Utc::now(),
    )
    .unwrap();
    assert_eq!(
        backup_reminder(home.path(), &project()).as_deref(),
        Some("backup: mode local (store: backup_never_confirmed); see engram backup status")
    );
    // A damaged configuration is still a configured target.
    let paths = RecordPaths::new(home.path(), &project(), CopyKind::Store);
    fs::write(&paths.config, b"{ damaged").unwrap();
    assert_eq!(
        backup_reminder(home.path(), &project()).as_deref(),
        Some("backup: mode local (store: backup_record_unreadable); see engram backup status")
    );
}

#[test]
fn a_qualifying_copy_on_disk_gives_no_line_until_its_last_push_fails() {
    let home = temp_home().unwrap();
    record_target(
        home.path(),
        &project(),
        true,
        Some(recent_attempt(AttemptOutcome::Uploaded, None)),
    );
    assert_eq!(backup_reminder(home.path(), &project()), None);

    let home = temp_home().unwrap();
    record_target(
        home.path(),
        &project(),
        true,
        Some(recent_attempt(
            AttemptOutcome::Failed,
            Some("backup_put_failed"),
        )),
    );
    assert_eq!(
        backup_reminder(home.path(), &project()).as_deref(),
        Some(
            "backup: the last store push failed: backup_put_failed; an earlier copy still qualifies (store copy: off-host asserted; not verified); see engram backup status"
        )
    );
}
