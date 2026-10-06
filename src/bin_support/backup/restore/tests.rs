use std::{fs, time::Duration};

use engram::{
    LocalWorkService, SessionId,
    backup::{
        restore::restore_record_path,
        status::backup_status,
        target::{AdapterKind, TargetRequest, set_target},
    },
};

use super::*;

#[test]
fn path_io_and_verification_failures_offer_their_own_remedies() {
    let path = Path::new("missing-copy.db");
    let error = engram::storage::open_sqlite_file(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap_err();
    assert_eq!(verification_code(&error), "backup_restore_path_unopenable");
    let failure = check_installed(path, &ProjectId("unused".into())).unwrap_err();
    assert!(failure.message.contains("restore stays pending"));
    // Missing-copy hashing may fail before SQLite opens it. Neither path
    // claims that a different build or a migration repairs the missing file.
    assert!(!failure.message.contains("migration") && !failure.message.contains("capturing build"));
    let io = io_failure(path, &io::Error::from_raw_os_error(5));
    assert!(io.message.contains("UTF-16 units") && io.message.contains("filesystem access"));
    assert_eq!(
        verification_code(&StoreError::DifferentBuildSchema),
        "backup_restore_format_unaccepted"
    );
}
use crate::{
    bin_support::backup::push::{Outcome, PushSettings, push},
    test_support::{TempHome, make_dir_link, remove_dir_link, temp_home},
};

/// An origin home with a store, a directory target and pushed copies, and a
/// clean home that configured the same target.
struct Fixture {
    origin: TempHome,
    clean: TempHome,
    project: ProjectId,
    copies: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let origin = temp_home().unwrap();
        let clean = temp_home().unwrap();
        let project = ProjectId("restore-unit-project".into());
        let database = engram::project_database_path(origin.path(), &project);
        fs::create_dir_all(database.parent().unwrap()).unwrap();
        drop(SqliteStore::open(&database).unwrap());
        let copies = origin.path().join("copies");
        fs::create_dir_all(&copies).unwrap();
        let fixture = Self {
            origin,
            clean,
            project,
            copies,
        };
        for home in [fixture.origin.path(), fixture.clean.path()] {
            set_target(
                home,
                &fixture.project,
                &TargetRequest {
                    kind: CopyKind::Store,
                    adapter: AdapterKind::Directory,
                    dir: fixture.copies.clone(),
                    disclosure_authorized_by: "greg".into(),
                    off_host_asserted_by: Some("greg".into()),
                    window_hours: 24,
                    keep: 5,
                },
                Utc::now(),
            )
            .unwrap();
        }
        fixture
    }

    /// Changes the origin store and pushes a copy of it; returns its name.
    fn push_change(&self, key: &str) -> String {
        LocalWorkService::new(
            engram::project_database_path(self.origin.path(), &self.project),
            self.project.clone(),
            "restore-test".into(),
            SessionId("restore-test".into()),
            None,
        )
        .remember_project_memory(
            format!("{key} body"),
            Some(key.into()),
            false,
            None,
            Utc::now(),
        )
        .unwrap();
        let run = push(
            self.origin.path(),
            &self.project,
            CopyKind::Store,
            &PushSettings::new(Duration::from_secs(120), Duration::from_secs(120)),
        );
        assert_eq!(run.report.outcome, Outcome::Uploaded, "{:?}", run.report);
        run.report.receipt.unwrap().manifest.copy
    }

    fn database(&self) -> PathBuf {
        engram::project_database_path(self.clean.path(), &self.project)
    }

    /// Fetches `copy` into `out` from the clean home.
    fn fetch(&self, copy: &str, out: &Path, leave_staging: bool) -> super::super::fetch::Fetched {
        let mut settings = ReadSettings::new(Duration::from_secs(120));
        settings.leave_staging = leave_staging;
        let run = super::super::fetch::fetch(
            self.clean.path(),
            &self.project,
            CopyKind::Store,
            copy,
            out,
            &settings,
        );
        assert!(run.abandoned.is_none());
        run.outcome.expect("the fetch succeeds")
    }

    fn restore(&self, copy: &str, stop: Option<Stop>) -> Result<Restored, ReadFailure> {
        let mut settings = RestoreSettings::new(ReadSettings::new(Duration::from_secs(120)));
        settings.stop = stop;
        let run = restore(
            self.clean.path(),
            &self.project,
            &self.database(),
            copy,
            Some("greg"),
            &settings,
        );
        assert!(run.abandoned.is_none());
        run.outcome
    }

    fn abandon(
        &self,
        copy: &str,
        by: &str,
        stop: Option<Stop>,
    ) -> Result<Abandonment, ReadFailure> {
        let mut settings = RestoreSettings::new(ReadSettings::new(Duration::from_secs(120)));
        settings.stop = stop;
        abandon_pending(
            self.clean.path(),
            &self.project,
            &self.database(),
            copy,
            Some(by),
            &settings,
        )
    }

    fn records(&self) -> RestoreRecords {
        read_restore_record(self.clean.path(), &self.project)
    }

    /// The files beside the restore record: the archives of abandoned
    /// restores among them.
    fn archives(&self) -> Vec<PathBuf> {
        let directory = restore_record_path(self.clean.path(), &self.project)
            .parent()
            .unwrap()
            .to_path_buf();
        let mut archives: Vec<_> = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("store.restore-abandoned-")
            })
            .collect();
        archives.sort();
        archives
    }

    fn record(&self) -> RestoreRecord {
        match read_restore_record(self.clean.path(), &self.project) {
            RestoreRecords::Recorded(record) => *record,
            other => panic!("{other:?}"),
        }
    }

    fn status_state(&self) -> Option<&'static str> {
        backup_status(self.clean.path(), &self.project, &self.database())
            .restore
            .map(|restore| restore.state)
    }

    fn store_directory_names(&self) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(self.database().parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

fn failure_code(outcome: Result<Restored, ReadFailure>) -> &'static str {
    match outcome {
        Ok(restored) => panic!("the restore must be refused: {restored:?}"),
        Err(failure) => failure.code,
    }
}

#[test]
fn a_restore_stopped_after_its_pending_record_leaves_no_store_and_a_retry_of_the_same_copy_finishes_it()
 {
    let fixture = Fixture::new();
    let first = fixture.push_change("first");
    let second = fixture.push_change("second");

    assert_eq!(
        failure_code(fixture.restore(&first, Some(Stop::AfterPending))),
        "test_stop"
    );
    // No store, the record pending, and only the staging file beside where
    // the store goes; status shows it pending and readiness finds no store.
    assert!(!fixture.database().exists());
    let pending = fixture.record();
    assert_eq!(pending.state, RestoreState::Pending);
    assert_eq!(pending.copy, first);
    assert_eq!(pending.origin_retired.by, "greg");
    let names = fixture.store_directory_names();
    assert_eq!(names.len(), 1, "{names:?}");
    assert!(names[0].starts_with(STAGING_PREFIX) && names[0].ends_with(STAGING_SUFFIX));
    assert_eq!(fixture.status_state(), Some("pending"));
    assert!(matches!(
        SqliteStore::readiness(&fixture.database(), None),
        Err(StoreError::StoreNotInitialized)
    ));

    // Another copy is refused while this one is pending, and changes nothing.
    assert_eq!(
        failure_code(fixture.restore(&second, None)),
        "backup_restore_pending_other"
    );
    assert_eq!(fixture.record(), pending);

    // The same copy finishes it, and the earlier staging file is gone.
    let restored = fixture.restore(&first, None).unwrap();
    assert!(!restored.completed_interrupted);
    assert_eq!(restored.copy, first);
    let completed = fixture.record();
    assert_eq!(completed.state, RestoreState::Completed);
    assert_eq!(completed.sha256, pending.sha256);
    assert_eq!(completed.occurrence_id, pending.occurrence_id);
    assert_eq!(completed.pending_at, pending.pending_at);
    assert_eq!(completed.origin_retired, pending.origin_retired);
    assert_eq!(fixture.store_directory_names(), ["engram.db"]);
    assert_eq!(fixture.status_state(), Some("restored"));
}

#[test]
fn a_restore_stopped_after_its_move_is_completed_by_a_retry_only_over_the_recorded_bytes() {
    let fixture = Fixture::new();
    let copy = fixture.push_change("first");

    assert_eq!(
        failure_code(fixture.restore(&copy, Some(Stop::AfterMove))),
        "test_stop"
    );
    assert_eq!(fixture.store_directory_names(), ["engram.db"]);
    assert_eq!(fixture.record().state, RestoreState::Pending);
    let pending_occurrence = fixture.record().occurrence_id;
    // Reading the status opens the store read-only, which leaves an empty
    // write-ahead log and its index beside it; those hold no rows.
    assert_eq!(fixture.status_state(), Some("pending"));
    assert_eq!(
        fixture.store_directory_names(),
        ["engram.db", "engram.db-shm", "engram.db-wal"]
    );
    let wal = PathBuf::from(format!("{}-wal", fixture.database().display()));
    assert_eq!(fs::metadata(&wal).unwrap().len(), 0);

    let restored = fixture.restore(&copy, None).unwrap();
    assert!(restored.completed_interrupted);
    assert!(
        report_text(&restored).starts_with(&format!(
            "completed an interrupted restore of {copy} into {}
",
            fixture.database().display()
        )),
        "{}",
        report_text(&restored)
    );
    assert_eq!(fixture.record().state, RestoreState::Completed);
    assert_eq!(fixture.record().occurrence_id, pending_occurrence);
    assert_eq!(fixture.status_state(), Some("restored"));

    // A store changed after the move is not the restore's own output.
    let changed = Fixture::new();
    let copy = changed.push_change("first");
    assert_eq!(
        failure_code(changed.restore(&copy, Some(Stop::AfterMove))),
        "test_stop"
    );
    let mut bytes = fs::read(changed.database()).unwrap();
    bytes.push(0);
    fs::write(changed.database(), &bytes).unwrap();
    let refusal = changed.restore(&copy, None).unwrap_err();
    assert_eq!(refusal.code, "backup_restore_store_exists");
    assert!(
        refusal
            .message
            .contains(&format!("pending restore of copy {copy}"))
            && refusal.message.contains("its SHA-256 is"),
        "{}",
        refusal.message
    );
    assert_eq!(fs::read(changed.database()).unwrap(), bytes);
    assert_eq!(changed.record().state, RestoreState::Pending);

    // So is a store beside a log with any bytes, or a rollback journal.
    for (suffix, bytes) in [("-wal", &b"frames"[..]), ("-journal", &b""[..])] {
        let logged = Fixture::new();
        let copy = logged.push_change("first");
        assert_eq!(
            failure_code(logged.restore(&copy, Some(Stop::AfterMove))),
            "test_stop"
        );
        let sidecar = PathBuf::from(format!("{}{suffix}", logged.database().display()));
        fs::write(&sidecar, bytes).unwrap();
        let refusal = logged.restore(&copy, None).unwrap_err();
        assert_eq!(refusal.code, "backup_restore_store_exists", "{suffix}");
        assert!(
            refusal
                .message
                .contains(&format!("pending restore of copy {copy}"))
                && refusal
                    .message
                    .contains(&format!("engram.db{suffix} stands beside")),
            "{}",
            refusal.message
        );
        assert_eq!(logged.record().state, RestoreState::Pending, "{suffix}");
    }
}

#[test]
fn a_completed_restore_whose_store_is_gone_is_kept_aside_by_the_next_one() {
    let fixture = Fixture::new();
    let first = fixture.push_change("first");
    let second = fixture.push_change("second");
    fixture.restore(&first, None).unwrap();
    let earlier = fixture.record();

    // Over the restored store, a second restore is refused.
    assert_eq!(
        failure_code(fixture.restore(&second, None)),
        "backup_restore_store_exists"
    );

    fs::remove_file(fixture.database()).unwrap();
    let restored = fixture.restore(&second, None).unwrap();
    let kept = restored
        .kept_record
        .expect("the earlier record is kept aside");
    let stamp = earlier
        .completed_at
        .unwrap()
        .format("%Y%m%dT%H%M%SZ")
        .to_string();
    assert_eq!(
        kept.file_name().unwrap().to_string_lossy(),
        format!("store.restore-{stamp}.json")
    );
    let kept_record: RestoreRecord = serde_json::from_slice(&fs::read(&kept).unwrap()).unwrap();
    assert_eq!(kept_record, earlier);
    assert_eq!(fixture.record().copy, second);
    assert_eq!(
        restore_record_path(fixture.clean.path(), &fixture.project).parent(),
        kept.parent()
    );
}

#[test]
fn repeated_restores_of_the_same_copy_have_distinct_occurrences() {
    let fixture = Fixture::new();
    let copy = fixture.push_change("same-copy");
    fixture.restore(&copy, None).unwrap();
    let first = fixture.record();
    fs::remove_file(fixture.database()).unwrap();
    let restored = fixture.restore(&copy, None).unwrap();
    let second = fixture.record();
    assert_ne!(first.occurrence_id, second.occurrence_id);
    assert_eq!(first.copy, second.copy);
    assert_eq!(first.sha256, second.sha256);
    let archived: RestoreRecord =
        serde_json::from_slice(&fs::read(restored.kept_record.unwrap()).unwrap()).unwrap();
    assert_eq!(archived, first);
}

#[test]
fn failed_pending_publication_creates_no_store_and_is_not_completed_evidence() {
    let fixture = Fixture::new();
    let copy = fixture.push_change("pending-write");
    assert_eq!(
        failure_code(fixture.restore(&copy, Some(Stop::FailPendingWrite))),
        "test_stop"
    );
    assert!(!fixture.database().exists());
    assert_eq!(fixture.records(), RestoreRecords::None);
    // No completed occurrence can authorize reopening; the host must retain maintenance.
    assert_eq!(fixture.status_state(), None);
    fixture.restore(&copy, None).unwrap();
    assert_eq!(fixture.record().state, RestoreState::Completed);
}

#[test]
fn a_failed_completion_write_keeps_the_occurrence_pending_until_retry() {
    let fixture = Fixture::new();
    let copy = fixture.push_change("completion-write");
    assert_eq!(
        failure_code(fixture.restore(&copy, Some(Stop::FailCompletionWrite))),
        "test_stop"
    );
    let pending = fixture.record();
    assert_eq!(pending.state, RestoreState::Pending);
    assert_eq!(fixture.status_state(), Some("pending"));
    fixture.restore(&copy, None).unwrap();
    let completed = fixture.record();
    assert_eq!(completed.occurrence_id, pending.occurrence_id);
    assert_eq!(completed.pending_at, pending.pending_at);
    assert_eq!(completed.state, RestoreState::Completed);
}

#[test]
fn a_new_session_s_claim_of_an_item_held_in_the_copy_is_refused_until_that_claim_expires() {
    use engram::{WorkProposeInput, WorkProposeResult, WorkUpdateInput};
    let fixture = Fixture::new();
    let start = Utc::now();
    let holder = LocalWorkService::new(
        engram::project_database_path(fixture.origin.path(), &fixture.project),
        fixture.project.clone(),
        "origin-agent".into(),
        SessionId("origin-holder".into()),
        None,
    );
    let proposed = holder
        .work_propose(
            WorkProposeInput::Root {
                acceptance_bindings: Vec::new(),
                evaluation_mode: None,
                external_ref: None,
                notes: Vec::new(),
                title: "Held at the cut".into(),
                outcome: "Held at the cut".into(),
                acceptance: vec!["it is held".into()],
                work_kind: None,
                priority: None,
                labels: Vec::new(),
                assigned_to: None,
                deferred_until: None,
                idempotency_key: "held-at-the-cut".into(),
            },
            start,
        )
        .unwrap();
    let WorkProposeResult::Root { work, .. } = proposed else {
        panic!("expected a root");
    };
    holder.work_focus(&work.short_ref, start).unwrap();
    holder
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(600),
                recovery_reason: None,
                idempotency_key: "hold".into(),
            },
            start,
        )
        .unwrap();
    let copy = fixture.push_change("after-the-claim");
    let restored = fixture.restore(&copy, None).unwrap();
    assert_eq!(restored.report.authority.unexpired_claims, 1);
    let expiry = restored.report.authority.claims_expire_by.unwrap();

    let newcomer = LocalWorkService::new(
        fixture.database(),
        fixture.project.clone(),
        "new-agent".into(),
        SessionId("new-session".into()),
        None,
    );
    let target = work.work_id.0.to_string();
    let claim = |key: &str, recovery: Option<&str>, at: chrono::DateTime<Utc>| {
        newcomer.work_update_on(
            Some(&target),
            WorkUpdateInput::Claim {
                ttl_seconds: None,
                recovery_reason: recovery.map(str::to_owned),
                idempotency_key: key.into(),
            },
            at,
        )
    };
    // Up to the last millisecond before the expiry the old claim holds.
    for (key, at) in [
        ("before", start + chrono::Duration::seconds(1)),
        ("last-moment", expiry - chrono::Duration::milliseconds(1)),
    ] {
        let error = claim(key, None, at).unwrap_err();
        assert!(
            matches!(error, StoreError::WorkClaimHeld { .. }),
            "{key}: {error}"
        );
    }
    // The report names the expiry to the millisecond, as the store keeps it;
    // the claim itself may run a fraction of a millisecond longer. After it
    // the claim no longer holds, and the item is recovered the ordinary way,
    // with an attributed reason.
    let after = expiry + chrono::Duration::milliseconds(1);
    let error = claim("after", None, after).unwrap_err();
    assert!(
        !matches!(error, StoreError::WorkClaimHeld { .. }),
        "{error}"
    );
    claim("recovered", Some("the origin store is retired"), after).unwrap();
}

#[test]
fn a_retry_that_cannot_fetch_again_keeps_the_earlier_staged_copy() {
    let fixture = Fixture::new();
    let copy = fixture.push_change("first");
    assert_eq!(
        failure_code(fixture.restore(&copy, Some(Stop::AfterPending))),
        "test_stop"
    );
    let staged = PathBuf::from(fixture.record().staging);
    assert!(staged.is_file());
    // The copy is gone from the target: the retry fails, and the only local
    // checked copy stays where it was.
    let project_dir = fixture
        .copies
        .join(engram::project_digest(&fixture.project));
    fs::remove_file(project_dir.join(format!("{copy}.manifest.json"))).unwrap();
    assert_eq!(
        failure_code(fixture.restore(&copy, None)),
        "backup_copy_unknown"
    );
    assert!(staged.is_file());
    assert_eq!(fixture.record().state, RestoreState::Pending);
}

#[test]
fn a_fetch_names_a_staging_file_its_move_left_behind() {
    let fixture = Fixture::new();
    let copy = fixture.push_change("leftover");
    let out_directory = fixture.clean.path().join("fetched");
    fs::create_dir_all(&out_directory).unwrap();

    // A move that takes its staging file leaves nothing to warn about.
    let clean = fixture.fetch(&copy, &out_directory.join("clean.db"), false);
    assert_eq!(clean.warnings, Vec::<String>::new());
    assert_eq!(
        crate::bin_support::backup_target::fetch_receipt(&clean)["warnings"],
        serde_json::json!([])
    );
    assert!(!crate::bin_support::backup_target::fetch_text(&clean).contains("warning:"));

    // A move that leaves it, as the hard-link fallback does when its unlink
    // fails, is detected by the move itself and named in the receipt.
    let out = out_directory.join("left.db");
    let fetched = fixture.fetch(&copy, &out, true);
    let staging = out_directory.join(format!(".left.db.{}.fetching", std::process::id()));
    assert!(staging.is_file(), "the staging file stays");
    assert!(out.is_file(), "the fetched copy is complete");
    assert_eq!(fetched.warnings.len(), 1, "{:?}", fetched.warnings);
    let named = std::path::absolute(&staging).unwrap().display().to_string();
    assert!(
        fetched.warnings[0].contains(&named),
        "{:?}",
        fetched.warnings
    );
    let receipt = crate::bin_support::backup_target::fetch_receipt(&fetched);
    assert_eq!(receipt["warnings"], serde_json::json!(fetched.warnings));
    let text = crate::bin_support::backup_target::fetch_text(&fetched);
    assert!(
        text.contains(&format!("warning: {}", fetched.warnings[0])),
        "{text}"
    );
}

#[test]
fn a_pending_restore_whose_copy_is_gone_is_abandoned_and_another_copy_then_restores() {
    let fixture = Fixture::new();
    let first = fixture.push_change("first");
    let second = fixture.push_change("second");
    assert_eq!(
        failure_code(fixture.restore(&first, Some(Stop::AfterPending))),
        "test_stop"
    );
    let pending = fixture.record();
    let staged = PathBuf::from(&pending.staging);
    assert!(staged.is_file());
    let original: serde_json::Value = serde_json::from_slice(
        &fs::read(restore_record_path(fixture.clean.path(), &fixture.project)).unwrap(),
    )
    .unwrap();

    // The copy is gone from the target: a retry cannot finish it, and another
    // copy is refused with both ways on, changing nothing.
    let project_dir = fixture
        .copies
        .join(engram::project_digest(&fixture.project));
    fs::remove_file(project_dir.join(format!("{first}.manifest.json"))).unwrap();
    assert_eq!(
        failure_code(fixture.restore(&first, None)),
        "backup_copy_unknown"
    );
    let other = fixture.restore(&second, None).unwrap_err();
    assert_eq!(other.code, "backup_restore_pending_other");
    for way in [
        format!("engram backup restore {first} --origin-retired-by=greg"),
        format!("engram backup restore {first} --abandon-pending --abandoned-by=NAME"),
    ] {
        assert!(other.message.contains(&way), "{}", other.message);
    }
    // Abandoning names its copy: another copy is refused the same way.
    let refused = fixture.abandon(&second, "greg", None).unwrap_err();
    assert_eq!(refused.code, "backup_restore_pending_other");
    assert_eq!(fixture.record(), pending);
    assert!(staged.is_file());

    // Files beside the staging file that are not this restore's stay.
    let directory = fixture.database().parent().unwrap().to_path_buf();
    let unrelated = [
        directory.join("notes.txt"),
        directory.join(format!(
            "{STAGING_PREFIX}{}{STAGING_SUFFIX}",
            uuid::Uuid::now_v7()
        )),
    ];
    for path in &unrelated {
        fs::write(path, b"not this restore's").unwrap();
    }

    let abandoned = fixture.abandon(&first, "Greg O'Neil", None).unwrap();
    assert_eq!(abandoned.copy, first);
    assert_eq!(abandoned.abandoned_by, "Greg O'Neil");
    assert!(abandoned.staging_removed);
    assert_eq!(abandoned.staging, staged);
    assert!(!staged.exists());
    for path in &unrelated {
        assert_eq!(fs::read(path).unwrap(), b"not this restore's");
    }
    // The pending record is archived whole and no longer active.
    assert_eq!(fixture.archives(), std::slice::from_ref(&abandoned.archive));
    let envelope: engram::backup::restore::AbandonedRestore =
        serde_json::from_slice(&fs::read(&abandoned.archive).unwrap()).unwrap();
    assert_eq!(envelope.abandoned.by, "Greg O'Neil");
    assert_eq!(envelope.abandoned.at, abandoned.abandoned_at);
    assert_eq!(envelope.pending, original);
    assert_eq!(fixture.records(), RestoreRecords::None);
    assert_eq!(fixture.status_state(), None);
    let receipt = crate::bin_support::backup_target::abandon_receipt(&abandoned);
    assert_eq!(receipt["abandoned"], serde_json::json!(first));
    assert_eq!(receipt["staging_removed"], serde_json::json!(true));
    assert_eq!(
        receipt["archive"],
        serde_json::json!(abandoned.archive.display().to_string())
    );
    let text = crate::bin_support::backup_target::abandon_text(&abandoned);
    assert!(
        text.starts_with(&format!(
            "abandoned the pending restore of {first} (by Greg O'Neil, asserted)"
        )),
        "{text}"
    );

    // Another copy now restores.
    let restored = fixture.restore(&second, None).unwrap();
    assert_eq!(restored.copy, second);
    assert_eq!(fixture.record().state, RestoreState::Completed);
}

#[test]
fn abandoning_needs_a_pending_restore_of_that_copy_a_named_operator_and_no_store() {
    let fixture = Fixture::new();
    let copy = fixture.push_change("first");
    assert_eq!(
        fixture.abandon(&copy, "greg", None).unwrap_err().code,
        "backup_restore_not_pending"
    );
    assert_eq!(
        failure_code(fixture.restore(&copy, Some(Stop::AfterPending))),
        "test_stop"
    );
    let pending = fixture.record();
    for by in ["", "  ", "greg\nsmith"] {
        assert_eq!(
            fixture.abandon(&copy, by, None).unwrap_err().code,
            "backup_restore_abandoner_unstated",
            "{by:?}"
        );
    }
    assert_eq!(fixture.record(), pending);

    // A store moved into place is never abandoned under it.
    let moved = Fixture::new();
    let copy = moved.push_change("first");
    assert_eq!(
        failure_code(moved.restore(&copy, Some(Stop::AfterMove))),
        "test_stop"
    );
    let refused = moved.abandon(&copy, "greg", None).unwrap_err();
    assert_eq!(refused.code, "backup_restore_store_exists");
    assert_eq!(moved.record().state, RestoreState::Pending);
    assert!(moved.database().is_file());

    // A completed restore is nothing to abandon.
    let completed = Fixture::new();
    let copy = completed.push_change("first");
    completed.restore(&copy, None).unwrap();
    fs::remove_file(completed.database()).unwrap();
    assert_eq!(
        completed.abandon(&copy, "greg", None).unwrap_err().code,
        "backup_restore_not_pending"
    );
    assert_eq!(completed.record().state, RestoreState::Completed);
}

#[test]
fn a_failed_cleanup_or_archive_leaves_the_restore_pending_to_abandon_again() {
    let fixture = Fixture::new();
    let copy = fixture.push_change("first");
    assert_eq!(
        failure_code(fixture.restore(&copy, Some(Stop::AfterPending))),
        "test_stop"
    );
    let pending = fixture.record();
    let staged = PathBuf::from(&pending.staging);

    // The staging file cannot be removed: nothing is archived, and the restore
    // stays pending with its staging file.
    let cleanup = fixture
        .abandon(&copy, "greg", Some(Stop::FailCleanup))
        .unwrap_err();
    assert_eq!(cleanup.code, "backup_io");
    assert!(
        cleanup
            .message
            .contains("stays pending and is not abandoned"),
        "{}",
        cleanup.message
    );
    assert!(staged.is_file());
    assert_eq!(fixture.record(), pending);
    assert_eq!(fixture.archives(), Vec::<PathBuf>::new());

    // The archive cannot be written after the staging file is removed: the
    // restore stays pending, and the refusal says the staging file is gone.
    let archive = fixture
        .abandon(&copy, "greg", Some(Stop::FailArchive))
        .unwrap_err();
    assert!(
        archive.message.contains(&format!(
            "its staging file {} was already removed",
            staged.display()
        )) && archive.message.contains("is not abandoned"),
        "{}",
        archive.message
    );
    assert!(!staged.exists());
    assert_eq!(fixture.record(), pending);
    assert_eq!(fixture.archives(), Vec::<PathBuf>::new());

    // Abandoning again finds the staging file already gone and finishes.
    let abandoned = fixture.abandon(&copy, "greg", None).unwrap();
    assert!(!abandoned.staging_removed);
    assert_eq!(fixture.archives(), [abandoned.archive]);
    assert_eq!(fixture.records(), RestoreRecords::None);
}

#[test]
fn a_retry_points_the_record_at_its_new_copy_before_it_removes_the_earlier_one() {
    let fixture = Fixture::new();
    let copy = fixture.push_change("first");
    assert_eq!(
        failure_code(fixture.restore(&copy, Some(Stop::AfterPending))),
        "test_stop"
    );
    let pending = fixture.record();
    let first = PathBuf::from(&pending.staging);
    let file_name = |path: &Path| path.file_name().unwrap().to_string_lossy().into_owned();

    // The record cannot be pointed at the new copy: the earlier staging file
    // and the record stay as they were, and the new copy is removed.
    let unwritten = fixture
        .restore(&copy, Some(Stop::FailPointerWrite))
        .unwrap_err();
    assert!(
        unwritten.message.contains(&format!(
            "the pending record and its staged copy {} stay as they were",
            first.display()
        )),
        "{}",
        unwritten.message
    );
    assert_eq!(fixture.record(), pending);
    assert_eq!(fixture.store_directory_names(), [file_name(&first)]);

    // A retry whose check is refused keeps its own new copy, and the record
    // names it; the earlier staging file is gone.
    assert_eq!(
        failure_code(fixture.restore(&copy, Some(Stop::FailCheck))),
        "backup_restore_check_failed"
    );
    let switched = fixture.record();
    let second = PathBuf::from(&switched.staging);
    assert_ne!(second, first);
    assert_eq!(switched.state, RestoreState::Pending);
    assert_eq!(switched.sha256, pending.sha256);
    assert_eq!(fixture.store_directory_names(), [file_name(&second)]);

    // Another retry finishes, and nothing of the earlier runs is left.
    fixture.restore(&copy, None).unwrap();
    assert_eq!(fixture.record().state, RestoreState::Completed);
    assert_eq!(fixture.store_directory_names(), ["engram.db"]);
}

/// A home with the project's store directory, and a staging file name.
struct StagingHome {
    home: TempHome,
    project: ProjectId,
    name: String,
}

impl StagingHome {
    fn new() -> Self {
        let home = temp_home().unwrap();
        let project = ProjectId("staging-unit-project".into());
        fs::create_dir_all(Self::store_directory_of(home.path(), &project)).unwrap();
        Self {
            home,
            project,
            name: format!("{STAGING_PREFIX}{}{STAGING_SUFFIX}", uuid::Uuid::now_v7()),
        }
    }

    fn store_directory_of(home: &Path, project: &ProjectId) -> PathBuf {
        home.join("projects").join(engram::project_digest(project))
    }

    fn store_directory(&self) -> PathBuf {
        Self::store_directory_of(self.home.path(), &self.project)
    }

    fn own(&self) -> PathBuf {
        self.store_directory().join(&self.name)
    }

    fn refusal(&self, staging: &Path) -> ReadFailure {
        match owned_staging(self.home.path(), &self.project, staging) {
            Ok(owned) => panic!("{} was taken: {}", staging.display(), owned.is_some()),
            Err(failure) => failure,
        }
    }
}

#[test]
fn only_the_recorded_own_staging_file_is_taken_for_removal() {
    let fixture = StagingHome::new();
    let directory = fixture.store_directory();
    let own = fixture.own();

    // Gone is allowed; a regular file is this restore's own, and is removed.
    assert!(
        owned_staging(fixture.home.path(), &fixture.project, &own)
            .unwrap()
            .is_none()
    );
    fs::write(&own, b"staged").unwrap();
    let settings = RestoreSettings::new(ReadSettings::new(Duration::from_secs(1)));
    owned_staging(fixture.home.path(), &fixture.project, &own)
        .unwrap()
        .expect("its own staging file")
        .remove(&settings)
        .unwrap();
    assert!(!own.exists());
    fs::write(&own, b"staged").unwrap();

    // Elsewhere, through `..`, another name, a directory: refused.
    let outside = fixture.home.path().join(&fixture.name);
    fs::write(&outside, b"elsewhere").unwrap();
    let traversal = directory
        .join("..")
        .join(engram::project_digest(&fixture.project))
        .join(&fixture.name);
    let unlike = directory.join("engram.db");
    fs::write(&unlike, b"a store").unwrap();
    let loose = directory.join(format!("{STAGING_PREFIX}x{STAGING_SUFFIX}"));
    fs::write(&loose, b"not a restore's name").unwrap();
    let folder = directory.join(format!(
        "{STAGING_PREFIX}{}{STAGING_SUFFIX}",
        uuid::Uuid::now_v7()
    ));
    fs::create_dir(&folder).unwrap();
    for path in [&outside, &traversal, &unlike, &loose, &folder] {
        assert_eq!(
            fixture.refusal(path).code,
            "backup_restore_staging_unowned",
            "{}",
            path.display()
        );
    }

    // A link at the staging name: refused.
    let target = fixture.home.path().join("link-target");
    fs::create_dir_all(&target).unwrap();
    let linked = directory.join(format!(
        "{STAGING_PREFIX}{}{STAGING_SUFFIX}",
        uuid::Uuid::now_v7()
    ));
    make_dir_link(&target, &linked);
    assert_eq!(
        fixture.refusal(&linked).code,
        "backup_restore_staging_unowned"
    );
    remove_dir_link(&linked);

    // Nothing was removed by any of these looks.
    for path in [&own, &outside, &unlike, &loose] {
        assert!(path.exists(), "{}", path.display());
    }
}

#[test]
fn a_staging_file_reached_through_a_linked_store_directory_or_projects_is_refused() {
    // The store's directory is a link to a directory elsewhere that holds a
    // file of the same name: refused, and that file stays.
    let fixture = StagingHome::new();
    let elsewhere = fixture.home.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::write(elsewhere.join(&fixture.name), b"not the restore's").unwrap();
    fs::remove_dir(fixture.store_directory()).unwrap();
    make_dir_link(&elsewhere, &fixture.store_directory());
    let refusal = fixture.refusal(&fixture.own());
    assert_eq!(refusal.code, "backup_restore_staging_unowned");
    assert!(refusal.message.contains("link"), "{}", refusal.message);
    assert_eq!(
        fs::read(elsewhere.join(&fixture.name)).unwrap(),
        b"not the restore's"
    );
    remove_dir_link(&fixture.store_directory());

    // So is one under `projects` linked elsewhere.
    let fixture = StagingHome::new();
    let digest = engram::project_digest(&fixture.project);
    let outside = fixture.home.path().join("outside-projects");
    fs::create_dir_all(outside.join(&digest)).unwrap();
    fs::write(
        outside.join(&digest).join(&fixture.name),
        b"not the restore's",
    )
    .unwrap();
    fs::remove_dir_all(fixture.home.path().join("projects")).unwrap();
    make_dir_link(&outside, &fixture.home.path().join("projects"));
    let refusal = fixture.refusal(&fixture.own());
    assert_eq!(refusal.code, "backup_restore_staging_unowned");
    assert!(refusal.message.contains("link"), "{}", refusal.message);
    assert_eq!(
        fs::read(outside.join(&digest).join(&fixture.name)).unwrap(),
        b"not the restore's"
    );
    remove_dir_link(&fixture.home.path().join("projects"));
}

/// Between the check and the removal, moves the store's directory aside and
/// puts a link to a directory elsewhere, holding a file of the same name, in
/// its place, where the operating system allows that.
fn swap_store_directory_for_a_link(name: &Path) {
    let home = SWAPPED_HOME.with(|home| home.borrow().clone()).unwrap();
    let directory =
        StagingHome::store_directory_of(&home, &ProjectId("staging-unit-project".into()));
    let elsewhere = home.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::write(elsewhere.join(name), b"not the restore's").unwrap();
    let moved = home.join("moved-store-directory");
    // Windows: the held directory cannot be renamed while it is open.
    let swapped = fs::rename(&directory, &moved).is_ok();
    if swapped {
        make_dir_link(&elsewhere, &directory);
    }
    SWAP.with(|swap| *swap.borrow_mut() = Some(swapped));
}

thread_local! {
    static SWAPPED_HOME: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
    static SWAP: std::cell::RefCell<Option<bool>> = const { std::cell::RefCell::new(None) };
}

#[test]
fn a_store_directory_swapped_for_a_link_after_the_check_never_redirects_the_removal() {
    let fixture = StagingHome::new();
    let own = fixture.own();
    fs::write(&own, b"staged").unwrap();
    SWAPPED_HOME.with(|home| *home.borrow_mut() = Some(fixture.home.path().to_path_buf()));
    let mut settings = RestoreSettings::new(ReadSettings::new(Duration::from_secs(1)));
    settings.before_removal = Some(swap_store_directory_for_a_link);
    let owned = owned_staging(fixture.home.path(), &fixture.project, &own)
        .unwrap()
        .expect("its own staging file");
    owned.remove(&settings).unwrap();
    let swapped = SWAP.with(|swap| *swap.borrow()).expect("the hook ran");
    // Windows: the held handles deny the rename; Unix allows it.
    assert_eq!(swapped, cfg!(unix));
    let elsewhere = fixture.home.path().join("elsewhere").join(&fixture.name);
    // The file elsewhere, behind the link, is never the one removed.
    assert_eq!(fs::read(&elsewhere).unwrap(), b"not the restore's");
    if swapped {
        // Unix: the removal went through the held directory, now moved.
        let moved = fixture
            .home
            .path()
            .join("moved-store-directory")
            .join(&fixture.name);
        assert!(!moved.exists());
        remove_dir_link(&fixture.store_directory());
    } else {
        // Windows: the directory could not be moved, and the file in it is
        // the one removed.
        assert!(!own.exists());
    }
}

#[test]
fn a_retry_whose_copy_does_not_hold_the_pending_bytes_keeps_the_earlier_staging_file() {
    use engram::backup::{
        restore::write_restore_record,
        target::{PushLock, RecordPaths},
    };
    let fixture = Fixture::new();
    let copy = fixture.push_change("first");
    assert_eq!(
        failure_code(fixture.restore(&copy, Some(Stop::AfterPending))),
        "test_stop"
    );
    // The pending record names other bytes than the target's copy holds.
    let mut pending = fixture.record();
    pending.sha256 = "00".repeat(32);
    let lock = PushLock::try_acquire(&RecordPaths::new(
        fixture.clean.path(),
        &fixture.project,
        CopyKind::Store,
    ))
    .unwrap();
    write_restore_record(fixture.clean.path(), &fixture.project, &lock, &pending).unwrap();
    drop(lock);
    let first = PathBuf::from(&pending.staging);
    let names = fixture.store_directory_names();

    let refusal = fixture.restore(&copy, None).unwrap_err();
    assert_eq!(refusal.code, "backup_restore_pending_mismatch");
    assert!(
        refusal.message.contains(&format!(
            "the pending record and its staged copy {} stay as they were",
            first.display()
        )),
        "{}",
        refusal.message
    );
    // The new copy is gone; the record and the earlier staging file stay.
    assert_eq!(fixture.record(), pending);
    assert_eq!(fixture.store_directory_names(), names);
    assert!(first.is_file());
}

/// The same path, reached as the local machine's administrative share, or
/// `None` where that share cannot be reached.
#[cfg(windows)]
fn through_the_admin_share(path: &Path) -> Option<PathBuf> {
    let text = path.to_str()?;
    let mut chars = text.chars();
    let drive = chars.next()?;
    let rest = text.strip_prefix(&format!("{drive}:\\"))?;
    let shared = PathBuf::from(format!(r"\\localhost\{drive}$\{rest}"));
    fs::metadata(&shared).is_ok().then_some(shared)
}

#[cfg(windows)]
#[test]
fn a_staging_file_under_a_network_home_is_removed_where_it_stands() {
    let fixture = StagingHome::new();
    let Some(home) = through_the_admin_share(fixture.home.path()) else {
        eprintln!("the administrative share is not reachable here; nothing to check");
        return;
    };
    let staging = home
        .join("projects")
        .join(engram::project_digest(&fixture.project))
        .join(&fixture.name);
    fs::write(fixture.own(), b"staged").unwrap();
    let settings = RestoreSettings::new(ReadSettings::new(Duration::from_secs(1)));
    owned_staging(&home, &fixture.project, &staging)
        .unwrap()
        .expect("its own staging file")
        .remove(&settings)
        .unwrap();
    // Removed where it stands, through the network path, not under a path
    // relative to the working directory.
    assert!(!fixture.own().exists());
}
