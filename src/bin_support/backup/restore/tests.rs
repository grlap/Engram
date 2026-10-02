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
use crate::{
    bin_support::backup::push::{Outcome, PushSettings, push},
    test_support::{TempHome, temp_home},
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
fn only_an_own_staging_file_is_removed_for_a_retry() {
    let home = temp_home().unwrap();
    let directory = home.path().join("store-directory");
    fs::create_dir_all(&directory).unwrap();
    let elsewhere = home
        .path()
        .join(format!("{STAGING_PREFIX}x{STAGING_SUFFIX}"));
    let unlike = directory.join("engram.db");
    let own = directory.join(format!("{STAGING_PREFIX}x{STAGING_SUFFIX}"));
    for path in [&elsewhere, &unlike, &own] {
        fs::write(path, b"kept unless it is the restore's own").unwrap();
    }
    for path in [&elsewhere, &unlike, &own] {
        remove_own_staging(&directory, path);
    }
    assert!(elsewhere.exists());
    assert!(unlike.exists());
    assert!(!own.exists());
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
