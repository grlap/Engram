use chrono::TimeZone;

use super::*;
use crate::test_support::temp_home;

fn project() -> ProjectId {
    ProjectId("restore-record-project".into())
}

fn at(second: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 2, 12, 0, 0)
        .single()
        .expect("fixed test timestamp")
        + chrono::Duration::seconds(second)
}

fn pending() -> RestoreRecord {
    RestoreRecord {
        format_version: RECORD_FORMAT_VERSION,
        project: project().0,
        copy: "20261002T120000Z-copy".into(),
        sha256: "ab".repeat(32),
        origin_host: Some("old-host".into()),
        origin_retired: Statement {
            by: "greg".into(),
            at: at(0),
        },
        staging: "staging".into(),
        state: RestoreState::Pending,
        pending_at: at(1),
        completed_at: None,
    }
}

#[test]
fn a_written_record_reads_back_and_a_missing_one_is_none() {
    let home = temp_home().unwrap();
    assert_eq!(
        read_restore_record(home.path(), &project()),
        RestoreRecords::None
    );
    let paths = RecordPaths::new(home.path(), &project(), CopyKind::Store);
    let lock = PushLock::try_acquire(&paths).unwrap();
    write_restore_record(home.path(), &project(), &lock, &pending()).unwrap();
    assert_eq!(
        read_restore_record(home.path(), &project()),
        RestoreRecords::Recorded(Box::new(pending()))
    );
    let mut completed = pending();
    completed.state = RestoreState::Completed;
    completed.completed_at = Some(at(2));
    write_restore_record(home.path(), &project(), &lock, &completed).unwrap();
    assert_eq!(
        read_restore_record(home.path(), &project()),
        RestoreRecords::Recorded(Box::new(completed))
    );
}

#[test]
fn a_record_that_cannot_be_used_is_unreadable_with_its_reason() {
    let home = temp_home().unwrap();
    let path = restore_record_path(home.path(), &project());
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let cases: Vec<(serde_json::Value, &str)> = vec![
        (
            {
                let mut value = serde_json::to_value(pending()).unwrap();
                value["project"] = "another".into();
                value
            },
            "another project",
        ),
        (
            {
                let mut value = serde_json::to_value(pending()).unwrap();
                value["format_version"] = 99.into();
                value
            },
            "format version 99",
        ),
        (
            {
                let mut value = serde_json::to_value(pending()).unwrap();
                value["completed_at"] = serde_json::to_value(at(2)).unwrap();
                value
            },
            "pending restore names a completion time",
        ),
        (
            {
                let mut value = serde_json::to_value(pending()).unwrap();
                value["state"] = "completed".into();
                value
            },
            "completed restore names no completion time",
        ),
    ];
    for (value, reason) in cases {
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        match read_restore_record(home.path(), &project()) {
            RestoreRecords::Unreadable {
                path: found,
                reason: found_reason,
            } => {
                assert_eq!(found, path);
                assert!(found_reason.contains(reason), "{found_reason}");
            }
            other => panic!("{other:?}"),
        }
    }
    fs::write(&path, b"not json").unwrap();
    assert!(matches!(
        read_restore_record(home.path(), &project()),
        RestoreRecords::Unreadable { .. }
    ));
}

#[test]
fn writing_needs_the_push_lock_of_this_project() {
    let home = temp_home().unwrap();
    let other = ProjectId("another-project".into());
    let lock =
        PushLock::try_acquire(&RecordPaths::new(home.path(), &other, CopyKind::Store)).unwrap();
    let error = write_restore_record(home.path(), &project(), &lock, &pending()).unwrap_err();
    assert_eq!(error.code(), "backup_target_invalid", "{error}");
    assert_eq!(
        read_restore_record(home.path(), &project()),
        RestoreRecords::None
    );
}

#[test]
fn a_completed_record_is_kept_aside_under_its_utc_completion_time_and_never_replaced() {
    let home = temp_home().unwrap();
    let paths = RecordPaths::new(home.path(), &project(), CopyKind::Store);
    let lock = PushLock::try_acquire(&paths).unwrap();
    let mut completed = pending();
    completed.state = RestoreState::Completed;
    completed.completed_at = Some(at(2));
    let error = keep_completed_record(home.path(), &project(), &lock, &pending()).unwrap_err();
    assert_eq!(error.code(), "backup_target_invalid", "{error}");

    write_restore_record(home.path(), &project(), &lock, &completed).unwrap();
    let kept = keep_completed_record(home.path(), &project(), &lock, &completed).unwrap();
    assert_eq!(
        kept.file_name().unwrap().to_string_lossy(),
        "store.restore-20261002T120002Z.json"
    );
    assert_eq!(
        read_restore_record(home.path(), &project()),
        RestoreRecords::None
    );
    let kept_record: RestoreRecord = serde_json::from_slice(&fs::read(&kept).unwrap()).unwrap();
    assert_eq!(kept_record, completed);

    write_restore_record(home.path(), &project(), &lock, &completed).unwrap();
    let second = keep_completed_record(home.path(), &project(), &lock, &completed).unwrap();
    assert_eq!(
        second.file_name().unwrap().to_string_lossy(),
        "store.restore-20261002T120002Z-1.json"
    );
    assert!(kept.exists());
}
