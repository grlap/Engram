use std::sync::{Mutex, mpsc};

use chrono::Duration as ChronoDuration;
use engram::{
    LocalWorkService, SessionId, SqliteStore,
    backup::{
        freshness::collect,
        status::{RunningBuild, build_status},
        target::{AdapterKind, TargetRequest, TargetState, set_target},
    },
};

use super::*;
use crate::{
    bin_support::backup::push::{PushSettings, push},
    test_support::{TempHome, temp_home},
};

struct Fixture {
    home: TempHome,
    project: ProjectId,
    copies: PathBuf,
}

impl Fixture {
    fn home(&self) -> &Path {
        self.home.path()
    }

    fn database(&self) -> PathBuf {
        engram::project_database_path(self.home(), &self.project)
    }

    fn paths(&self) -> RecordPaths {
        RecordPaths::new(self.home(), &self.project, CopyKind::Store)
    }

    fn push(&self) {
        let run = push(
            self.home(),
            &self.project,
            CopyKind::Store,
            &PushSettings::new(Duration::from_secs(120), Duration::from_secs(120)),
        );
        assert_eq!(
            run.report.outcome,
            crate::bin_support::backup::push::Outcome::Uploaded,
            "{:?}",
            run.report
        );
    }

    fn change(&self, key: &str) {
        LocalWorkService::new(
            self.database(),
            self.project.clone(),
            "check-test".into(),
            SessionId("check-test".into()),
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
    }

    fn state_bytes(&self) -> Vec<u8> {
        fs::read(self.paths().state).unwrap()
    }

    fn state(&self) -> TargetState {
        serde_json::from_slice(&self.state_bytes()).unwrap()
    }

    fn check(&self, settings: &CheckSettings, now: DateTime<Utc>) -> CheckRun {
        check_targets(self.home(), &self.project, settings, now)
    }

    /// The reason the store kind gives, read from local evidence at `now`.
    fn reason(&self, now: DateTime<Utc>) -> Option<&'static str> {
        let collected = collect(self.home(), &self.project, &self.database());
        build_status(&collected, &RunningBuild::current(), now).kinds[0].reason
    }

    fn data_file(&self) -> PathBuf {
        let copy = self.state().newest_receipt.unwrap().manifest.copy;
        self.copies
            .join(engram::project_digest(&self.project))
            .join(format!("{copy}.db.gz"))
    }
}

/// A store with one memory, a directory target, and one pushed copy.
fn fixture() -> Fixture {
    let home = temp_home().unwrap();
    let project = ProjectId("check-project".into());
    let database = engram::project_database_path(home.path(), &project);
    fs::create_dir_all(database.parent().unwrap()).unwrap();
    drop(SqliteStore::open(&database).unwrap());
    let copies = home.path().join("copies");
    fs::create_dir_all(&copies).unwrap();
    let fixture = Fixture {
        home,
        project,
        copies,
    };
    fixture.change("first");
    set_target(
        fixture.home(),
        &fixture.project,
        &TargetRequest {
            kind: CopyKind::Store,
            adapter: AdapterKind::Directory,
            dir: fixture.copies.clone(),
            disclosure_authorized_by: "greg".into(),
            off_host_asserted_by: Some("greg".into()),
            window_hours: 24,
            keep: 3,
        },
        Utc::now(),
    )
    .unwrap();
    fixture.push();
    fixture
}

fn settings() -> CheckSettings {
    CheckSettings::new(Duration::from_secs(120))
}

#[test]
fn a_copy_the_target_no_longer_holds_is_recorded_and_reads_copy_missing_at_once() {
    let fixture = fixture();
    let now = Utc::now();
    assert_eq!(fixture.reason(now), None);
    fs::remove_file(fixture.data_file()).unwrap();

    let run = fixture.check(&settings(), now);
    let report = &run.reports[0];
    assert_eq!(report.outcome, CheckOutcome::Missing);
    assert_eq!(report.code, Some("backup_copy_missing"));
    assert!(report.recorded, "{report:?}");
    let missing = fixture.state().missing_copy.unwrap();
    assert_eq!(missing.at, now);
    assert_eq!(fixture.reason(now), Some("backup_copy_missing"));
}

#[test]
fn an_unreachable_target_is_reported_beside_the_evidence_without_changing_it() {
    let fixture = fixture();
    let before = fixture.state_bytes();
    let away = fixture.home().join("copies-away");
    fs::rename(&fixture.copies, &away).unwrap();

    let run = fixture.check(&settings(), Utc::now());
    let report = &run.reports[0];
    assert_eq!(report.outcome, CheckOutcome::Unreachable);
    assert_eq!(report.code, Some("backup_target_unreachable"));
    assert!(!report.recorded);
    assert!(run.abandoned.is_none());
    assert_eq!(fixture.state_bytes(), before);
    assert!(!fixture.copies.exists());
    assert_eq!(fixture.reason(Utc::now()), None);
}

#[test]
fn a_check_that_reached_the_target_and_passed_its_deadline_times_out_without_changing_anything() {
    let fixture = fixture();
    let before = fixture.state_bytes();
    let (release, held) = mpsc::channel::<()>();
    let held = Arc::new(Mutex::new(held));
    let mut hurried = CheckSettings::new(Duration::from_millis(300));
    hurried.after_reach = Some(Arc::new(move || {
        let _ = held.lock().unwrap().recv();
    }));

    let run = fixture.check(&hurried, Utc::now());
    let report = &run.reports[0];
    assert_eq!(report.outcome, CheckOutcome::TimedOut);
    assert_eq!(report.code, Some("backup_check_timed_out"));
    assert!(!report.recorded);
    assert_eq!(fixture.state_bytes(), before);
    release.send(()).unwrap();
    run.abandoned
        .expect("the worker passed its deadline")
        .join()
        .unwrap();
    // The worker's late answer records nothing either.
    assert_eq!(fixture.state_bytes(), before);
}

#[test]
fn a_check_records_nothing_when_a_push_recorded_a_newer_receipt_in_between() {
    let fixture = Arc::new(fixture());
    let raced = Arc::clone(&fixture);
    let mut racing = settings();
    racing.before_record = Some(Arc::new(move || {
        // Between the check's read and its write, a push records a newer
        // receipt for a changed store.
        raced.change("raced");
        raced.push();
    }));
    let checked = fixture.state().newest_receipt.unwrap();

    let run = fixture.check(&racing, Utc::now());
    let report = &run.reports[0];
    assert_eq!(report.outcome, CheckOutcome::Confirmed);
    assert!(!report.recorded, "{report:?}");
    assert!(report.not_recorded.as_deref().unwrap().contains("changed"));
    let after = fixture.state();
    let newest = after.newest_receipt.clone().unwrap();
    assert_ne!(newest.manifest.copy, checked.manifest.copy);
    // The newest copy's evidence is the push's own, untouched by the check.
    assert_eq!(after.last_confirmation.unwrap().at, newest.at);
}

#[test]
fn past_the_window_the_mode_falls_to_local_and_a_check_alone_leaves_the_copy_stale() {
    let fixture = fixture();
    let start = Utc::now();
    assert_eq!(fixture.reason(start), None);
    let collected = collect(fixture.home(), &fixture.project, &fixture.database());
    let running = RunningBuild::current();
    assert_eq!(
        build_status(&collected, &running, start).durability.mode,
        engram::backup::freshness::Mode::LocalBackedUp
    );

    // A day and an hour later, with no new push.
    let later = start + ChronoDuration::hours(25);
    let status = build_status(&collected, &running, later);
    assert_eq!(
        status.durability.mode,
        engram::backup::freshness::Mode::Local
    );
    assert_eq!(status.kinds[0].reason, Some("backup_confirmation_expired"));

    // A check then confirms that old copy, without a new capture.
    let run = fixture.check(&settings(), later);
    assert_eq!(run.reports[0].outcome, CheckOutcome::Confirmed);
    assert!(run.reports[0].recorded);
    assert_eq!(fixture.state().last_confirmation.unwrap().at, later);
    let status = build_status(
        &collect(fixture.home(), &fixture.project, &fixture.database()),
        &running,
        later,
    );
    assert_eq!(
        status.durability.mode,
        engram::backup::freshness::Mode::Local
    );
    assert_eq!(status.kinds[0].reason, Some("backup_stale"));
}

#[test]
fn a_kind_without_a_copy_of_its_own_has_nothing_to_check() {
    let home = temp_home().unwrap();
    let run = check_targets(
        home.path(),
        &ProjectId("nothing".into()),
        &settings(),
        Utc::now(),
    );
    assert_eq!(run.reports[0].outcome, CheckOutcome::NothingToCheck);
    assert!(!home.path().join("backup-records").exists());
}
