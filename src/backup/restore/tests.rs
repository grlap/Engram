use std::{io, path::Path};

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
    static OCCURRENCE: std::sync::OnceLock<uuid::Uuid> = std::sync::OnceLock::new();
    RestoreRecord {
        format_version: RECORD_FORMAT_VERSION,
        occurrence_id: *OCCURRENCE.get_or_init(uuid::Uuid::now_v7),
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
fn a_nil_occurrence_cannot_replace_the_written_record() {
    let home = temp_home().unwrap();
    let paths = RecordPaths::new(home.path(), &project(), CopyKind::Store);
    let lock = PushLock::try_acquire(&paths).unwrap();
    let record = pending();
    write_restore_record(home.path(), &project(), &lock, &record).unwrap();
    let path = restore_record_path(home.path(), &project());
    let bytes = fs::read(&path).unwrap();
    let mut invalid = record;
    invalid.occurrence_id = uuid::Uuid::nil();
    assert!(matches!(
        write_restore_record(home.path(), &project(), &lock, &invalid),
        Err(TargetError::Invalid { .. })
    ));
    assert_eq!(fs::read(path).unwrap(), bytes);
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
fn missing_invalid_or_nil_occurrence_ids_stay_unreadable_without_mutation() {
    let home = temp_home().unwrap();
    let path = restore_record_path(home.path(), &project());
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    for occurrence in [
        None,
        Some(""),
        Some("not-a-uuid"),
        Some("00000000-0000-0000-0000-000000000000"),
    ] {
        let mut value = serde_json::to_value(pending()).unwrap();
        match occurrence {
            None => {
                value.as_object_mut().unwrap().remove("occurrence_id");
            }
            Some(id) => value["occurrence_id"] = id.into(),
        }
        let bytes = serde_json::to_vec(&value).unwrap();
        fs::write(&path, &bytes).unwrap();
        let RestoreRecords::Unreadable { reason, .. } =
            read_restore_record(home.path(), &project())
        else {
            panic!("invalid occurrence must refuse");
        };
        if occurrence.is_none() {
            assert!(
                reason.contains("legacy restore record has no occurrence_id"),
                "{reason}"
            );
        }
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
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

#[test]
fn an_abandoned_pending_record_is_archived_whole_under_a_new_name_and_cleared() {
    assert_abandoned_archive(temp_home().unwrap().path());
}

fn assert_abandoned_archive(home: &Path) {
    let paths = RecordPaths::new(home, &project(), CopyKind::Store);
    let lock = PushLock::try_acquire(&paths).unwrap();
    // Nothing pending: nothing is archived.
    assert!(matches!(
        archive_abandoned_pending(home, &project(), &lock, "greg", at(5)),
        Err(AbandonError::Archive(_))
    ));
    // A completed record is never abandoned.
    let mut completed = pending();
    completed.state = RestoreState::Completed;
    completed.completed_at = Some(at(2));
    write_restore_record(home, &project(), &lock, &completed).unwrap();
    assert!(matches!(
        archive_abandoned_pending(home, &project(), &lock, "greg", at(5)),
        Err(AbandonError::Archive(_))
    ));
    assert_eq!(
        read_restore_record(home, &project()),
        RestoreRecords::Recorded(Box::new(completed))
    );

    // Names already taken in that second are passed over, and left alone.
    let taken = [
        "store.restore-abandoned-20261002T120005Z.json",
        "store.restore-abandoned-20261002T120005Z-1.json",
    ];
    for name in taken {
        fs::write(paths.directory.join(name), name).unwrap();
    }
    write_restore_record(home, &project(), &lock, &pending()).unwrap();
    let original: serde_json::Value =
        serde_json::from_slice(&fs::read(restore_record_path(home, &project())).unwrap()).unwrap();
    let archived =
        archive_abandoned_pending(home, &project(), &lock, "Greg O'Neil", at(5)).unwrap();
    assert_eq!(archived.warnings, Vec::<String>::new());
    let archive = archived.archive;
    assert_eq!(archive.parent(), Some(paths.directory.as_path()));
    assert_eq!(
        archive.file_name().unwrap().to_string_lossy(),
        "store.restore-abandoned-20261002T120005Z-2.json"
    );
    for name in taken {
        assert_eq!(
            fs::read(paths.directory.join(name)).unwrap(),
            name.as_bytes()
        );
    }
    let envelope: AbandonedRestore = serde_json::from_slice(&fs::read(&archive).unwrap()).unwrap();
    assert_eq!(envelope.format_version, RECORD_FORMAT_VERSION);
    assert_eq!(
        envelope.abandoned,
        Statement {
            by: "Greg O'Neil".into(),
            at: at(5),
        }
    );
    // The pending record is kept as it was parsed, every field of it.
    assert_eq!(envelope.pending, original);
    let kept: RestoreRecord = serde_json::from_value(envelope.pending).unwrap();
    assert_eq!(kept, pending());
    // The active record is cleared: no restore is pending any more.
    assert_eq!(read_restore_record(home, &project()), RestoreRecords::None);
}

#[cfg(windows)]
#[test]
fn long_archive_paths_keep_collision_records_and_accept_relative_and_extended_homes() {
    use std::os::windows::ffi::OsStrExt;

    let root = temp_home().unwrap();
    for desired in [260, 400] {
        let mut home = root.path().join(format!("zażółć 🦀-{desired}"));
        let length = |home: &Path| {
            RecordPaths::new(home, &project(), CopyKind::Store)
                .directory
                .join(abandoned_name(at(5), 2))
                .as_os_str()
                .encode_wide()
                .count()
        };
        let minimum = length(&home);
        while length(&home) < desired {
            let remaining = desired - length(&home);
            if remaining <= 60 {
                let mut name = home.file_name().unwrap().to_os_string();
                name.push("x".repeat(remaining));
                home.set_file_name(name);
            } else {
                home.push("x".repeat(59));
            }
        }
        assert!(length(&home) >= desired);
        if minimum <= desired {
            assert_eq!(length(&home), desired);
        }
        assert_abandoned_archive(&home);
    }
    let extended = fs::canonicalize(root.path()).unwrap().join("extended");
    assert_abandoned_archive(&extended);
    let current = std::env::current_dir().unwrap();
    let relative = root.path().strip_prefix(&current).unwrap().join("relative");
    assert_abandoned_archive(&relative);
}

#[test]
fn the_ways_on_from_a_pending_restore_quote_what_a_shell_would_split() {
    assert_eq!(
        pending_ways_on("20261002T120000Z-copy", Some("greg")),
        "run `engram backup restore 20261002T120000Z-copy --origin-retired-by=greg` again to finish it, or `engram backup restore 20261002T120000Z-copy --abandon-pending --abandoned-by=NAME` to abandon it"
    );
    assert_eq!(
        pending_ways_on("odd copy", Some("Greg O'Neil")),
        "run `engram backup restore 'odd copy' --origin-retired-by='Greg O'\"'\"'Neil'` again to finish it, or `engram backup restore 'odd copy' --abandon-pending --abandoned-by=NAME` to abandon it"
    );
    assert!(pending_ways_on("c", None).contains("--origin-retired-by=NAME"));
}

/// The commands of the ways on from a pending restore.
fn ways_on_commands(copy: &str, by: &str) -> Vec<String> {
    let commands = pending_commands(copy, Some(by));
    let sentence = pending_ways_on(copy, Some(by));
    for command in &commands {
        assert!(sentence.contains(command.as_str()), "{sentence}");
    }
    commands.to_vec()
}

thread_local! {
    static LEAVE_TEMPORARY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FAIL_TEMPORARY_REMOVAL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Puts the temporary file back beside the archive after the move, as the
/// hard-link fallback leaves it when its unlink fails.
pub(super) fn after_archive_move(archive: &Path, temporary: &Path) {
    if LEAVE_TEMPORARY.get() {
        fs::copy(archive, temporary).unwrap();
    }
}

/// Removes a leftover temporary file, or fails without removing it when a
/// test asks.
pub(super) fn remove_temporary(path: &Path) -> io::Result<()> {
    if FAIL_TEMPORARY_REMOVAL.get() {
        Err(io::Error::other("stopped by a test"))
    } else {
        fs::remove_file(path)
    }
}

#[test]
fn a_temporary_file_the_archive_move_leaves_is_removed_or_named() {
    let home = temp_home().unwrap();
    let paths = RecordPaths::new(home.path(), &project(), CopyKind::Store);
    let lock = PushLock::try_acquire(&paths).unwrap();
    let temporaries = || {
        fs::read_dir(&paths.directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| {
                Path::new(name)
                    .extension()
                    .is_some_and(|extension| extension == "tmp")
            })
            .collect::<Vec<_>>()
    };
    LEAVE_TEMPORARY.set(true);
    write_restore_record(home.path(), &project(), &lock, &pending()).unwrap();
    let removed = archive_abandoned_pending(home.path(), &project(), &lock, "greg", at(5)).unwrap();
    assert_eq!(removed.warnings, Vec::<String>::new());
    assert_eq!(temporaries(), Vec::<String>::new());

    FAIL_TEMPORARY_REMOVAL.set(true);
    write_restore_record(home.path(), &project(), &lock, &pending()).unwrap();
    let named = archive_abandoned_pending(home.path(), &project(), &lock, "greg", at(6)).unwrap();
    let left = temporaries();
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(named.warnings.len(), 1, "{:?}", named.warnings);
    assert!(named.warnings[0].contains(&left[0]), "{:?}", named.warnings);
    // The archive stands and the record is cleared all the same.
    assert!(named.archive.is_file());
    assert_eq!(
        read_restore_record(home.path(), &project()),
        RestoreRecords::None
    );
    LEAVE_TEMPORARY.set(false);
    FAIL_TEMPORARY_REMOVAL.set(false);
}

const HOSTILE_NAME: &str =
    "Greg O'Neil’s ‘x’ $name $(printf injected) `printf injected`; | & > # \"“”";

#[cfg(unix)]
#[test]
fn the_ways_on_round_trip_a_hostile_name_through_a_posix_shell() {
    for name in [HOSTILE_NAME, DOTTED_NAME] {
        for (command, expected) in ways_on_commands("odd copy", name).into_iter().zip([
            vec![
                "engram".to_owned(),
                "backup".into(),
                "restore".into(),
                "odd copy".into(),
                format!("--origin-retired-by={name}"),
            ],
            vec![
                "engram".to_owned(),
                "backup".into(),
                "restore".into(),
                "odd copy".into(),
                "--abandon-pending".into(),
                "--abandoned-by=NAME".into(),
            ],
        ]) {
            // set collects the arguments instead of running anything.
            let output = std::process::Command::new("sh")
                .args(["-c", &format!("set -- {command}; printf '%s\\n' \"$@\"")])
                .output()
                .unwrap();
            assert!(output.status.success(), "{command}");
            let printed = String::from_utf8(output.stdout).unwrap();
            assert_eq!(printed.lines().collect::<Vec<_>>(), expected, "{command}");
        }
    }
}

#[cfg(windows)]
#[test]
fn the_ways_on_round_trip_a_hostile_name_through_powershell() {
    // Parse, never run, the command. SafeGetValue accepts only constant
    // values, so an interpolated expression also fails the test.
    let script = r"
$ErrorActionPreference = 'Stop'
$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseInput($env:ENGRAM_TEST_COMMAND, [ref]$tokens, [ref]$errors)
if ($errors.Count -ne 0 -or $ast.EndBlock.Statements.Count -ne 1) { throw 'not one literal command' }
$pipeline = $ast.EndBlock.Statements[0]
if ($pipeline.PipelineElements.Count -ne 1) { throw 'unexpected pipeline' }
$command = $pipeline.PipelineElements[0]
if ($command.Redirections.Count -ne 0) { throw 'unexpected redirection' }
$values = @($command.CommandElements | ForEach-Object { $_.SafeGetValue() })
ConvertTo-Json -Compress -EscapeHandling EscapeNonAscii -InputObject $values
";
    for name in [HOSTILE_NAME, DOTTED_NAME] {
        for (command, expected) in ways_on_commands("odd copy", name).into_iter().zip([
            serde_json::json!([
                "engram",
                "backup",
                "restore",
                "odd copy",
                format!("--origin-retired-by={name}")
            ]),
            serde_json::json!([
                "engram",
                "backup",
                "restore",
                "odd copy",
                "--abandon-pending",
                "--abandoned-by=NAME"
            ]),
        ]) {
            let output = std::process::Command::new("pwsh")
                .args(["-NoProfile", "-NonInteractive", "-Command", script])
                .env("ENGRAM_TEST_COMMAND", &command)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{command}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let values: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(values, expected, "{command}");
        }
    }
}

/// A plausible operator name with characters a shell may split at.
const DOTTED_NAME: &str = "greg.lapinski:ops/+x";
