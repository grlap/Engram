use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

use chrono::Utc;
use engram::{
    LocalWorkService, ObjectId, ProjectId, SessionId, SqliteStore,
    backup::{
        CopyKind,
        record::{Attempt, AttemptOutcome},
        target::{
            AdapterKind, PushLock, RecordPaths, TargetRequest, TargetState, TargetView,
            read_for_push, set_target, write_state,
        },
    },
};

use super::{Outcome, PushRun, PushSettings, push};
use crate::{
    bin_support::backup::directory::copy_name,
    test_support::{TempHome, make_dir_link, remove_dir_link, temp_home},
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

    fn paths(&self) -> RecordPaths {
        RecordPaths::new(self.home(), &self.project, CopyKind::Store)
    }

    /// The copies directory of this project at the target `root`.
    fn project_dir(&self, root: &Path) -> PathBuf {
        root.join(engram::project_digest(&self.project))
    }

    fn set(&self, root: &Path, authorized_by: &str) -> TargetView {
        set_target(
            self.home(),
            &self.project,
            &TargetRequest {
                kind: CopyKind::Store,
                adapter: AdapterKind::Directory,
                dir: root.to_path_buf(),
                disclosure_authorized_by: authorized_by.into(),
                off_host_asserted_by: Some("greg".into()),
                window_hours: 24,
                keep: 3,
            },
            Utc::now(),
        )
        .unwrap()
    }

    fn state(&self) -> TargetState {
        let paths = self.paths();
        let lock = PushLock::try_acquire(&paths).unwrap();
        read_for_push(&paths, &self.project, CopyKind::Store, &lock)
            .unwrap()
            .unwrap()
            .state
    }

    fn write(&self, state: &TargetState) {
        let paths = self.paths();
        let lock = PushLock::try_acquire(&paths).unwrap();
        write_state(&paths, &lock, state).unwrap();
    }

    /// Changes what the store holds, so the next capture has other bytes.
    fn change(&self, label: &str) {
        remember(self.home(), &self.project, label);
    }

    fn push(&self) -> PushRun {
        self.push_with(&settings())
    }

    fn push_with(&self, settings: &PushSettings) -> PushRun {
        push(self.home(), &self.project, CopyKind::Store, settings)
    }

    /// The names under this project's directory at `root`, sorted.
    fn files(&self, root: &Path) -> Vec<String> {
        let mut names: Vec<_> = match fs::read_dir(self.project_dir(root)) {
            Ok(entries) => entries
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("{error}"),
        };
        names.sort();
        names
    }

    /// The directories left in this project's capture stage.
    fn stages(&self) -> Vec<PathBuf> {
        let root = self
            .home()
            .join(engram::backup::STAGE_DIRECTORY)
            .join(engram::project_digest(&self.project));
        match fs::read_dir(root) {
            Ok(entries) => entries.map(|entry| entry.unwrap().path()).collect(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("{error}"),
        }
    }
}

fn remember(home: &Path, project: &ProjectId, key: &str) {
    let service = LocalWorkService::new(
        engram::project_database_path(home, project),
        project.clone(),
        "push-test".into(),
        SessionId("push-test".into()),
        None,
    );
    service
        .remember_project_memory(
            format!("{key} body"),
            Some(key.into()),
            false,
            None,
            Utc::now(),
        )
        .unwrap();
}

/// A store with one memory and a directory target configured for it.
fn fixture() -> Fixture {
    let home = temp_home().unwrap();
    let project = ProjectId("push-project".into());
    let database = engram::project_database_path(home.path(), &project);
    fs::create_dir_all(database.parent().unwrap()).unwrap();
    drop(SqliteStore::open(&database).unwrap());
    remember(home.path(), &project, "first");
    let copies = home.path().join("copies");
    fs::create_dir_all(&copies).unwrap();
    let fixture = Fixture {
        home,
        project,
        copies,
    };
    fixture.set(&fixture.copies, "greg");
    fixture
}

fn settings() -> PushSettings {
    PushSettings::new(Duration::from_secs(120), Duration::from_secs(120))
}

fn data(copy: &str) -> String {
    format!("{copy}.db.gz")
}

fn manifest(copy: &str) -> String {
    format!("{copy}.manifest.json")
}

/// An attempt for `copy` of the newest receipt's capture, as a push records
/// it before its put.
fn attempt_like(state: &TargetState, id: uuid::Uuid) -> Attempt {
    let mut manifest = state.newest_receipt.as_ref().unwrap().manifest.clone();
    manifest.copy = copy_name(manifest.capture.capture_started_at, id);
    Attempt {
        id,
        data_file: data(&manifest.copy),
        temporary_data_file: format!(".{}.tmp", data(&manifest.copy)),
        manifest,
    }
}

#[test]
fn a_push_puts_a_copy_and_records_its_receipt_then_confirms_an_unchanged_store() {
    let fixture = fixture();
    let first = fixture.push();
    assert_eq!(
        first.report.outcome,
        Outcome::Uploaded,
        "{:?}",
        first.report
    );
    assert!(first.abandoned.is_none());
    let state = fixture.state();
    let receipt = state.newest_receipt.clone().unwrap();
    let copy = receipt.manifest.copy.clone();
    assert_eq!(first.report.receipt.as_ref(), Some(&receipt));
    assert_eq!(state.receipts, std::slice::from_ref(&receipt));
    assert_eq!(state.pending, None);
    assert_eq!(
        state.observed_equal_at,
        Some(receipt.manifest.capture.capture_started_at)
    );
    let last = state.last_attempt.clone().unwrap();
    assert_eq!(last.outcome, AttemptOutcome::Uploaded);
    assert_eq!((last.code, last.message), (None, None));
    assert_eq!(
        fixture.files(&fixture.copies),
        [data(&copy), manifest(&copy)]
    );
    // The stage is gone.
    assert_eq!(fixture.stages(), Vec::<PathBuf>::new());

    // The same store again: nothing is uploaded, the copy's manifest at the
    // target stays as it was, and the store's content was observed in the
    // copy at the new capture's start.
    let manifest_path = fixture.project_dir(&fixture.copies).join(manifest(&copy));
    let stored_manifest = fs::read(&manifest_path).unwrap();
    let second = fixture.push();
    assert_eq!(
        second.report.outcome,
        Outcome::Unchanged,
        "{:?}",
        second.report
    );
    let after = fixture.state();
    assert_eq!(after.newest_receipt.as_ref(), Some(&receipt));
    assert_eq!(after.receipts, std::slice::from_ref(&receipt));
    assert!(after.observed_equal_at > state.observed_equal_at);
    assert_eq!(
        after.last_attempt.unwrap().outcome,
        AttemptOutcome::Unchanged
    );
    assert_eq!(
        fixture.files(&fixture.copies),
        [data(&copy), manifest(&copy)]
    );
    assert_eq!(fs::read(&manifest_path).unwrap(), stored_manifest);
}

#[test]
fn a_pending_copy_that_arrived_is_recorded_as_confirmed_without_being_put_again() {
    let fixture = fixture();
    assert_eq!(fixture.push().report.outcome, Outcome::Uploaded);
    let mut state = fixture.state();
    let receipt = state.newest_receipt.clone().unwrap();
    let copy = receipt.manifest.copy.clone();
    let directory = fixture.project_dir(&fixture.copies);
    let stored = fs::read(directory.join(data(&copy))).unwrap();
    let modified = fs::metadata(directory.join(data(&copy)))
        .unwrap()
        .modified()
        .unwrap();
    // The receipt is lost: the state holds the attempt as pending again.
    let id = uuid::Uuid::parse_str(copy.split_once('-').unwrap().1).unwrap();
    state.pending = Some(Attempt {
        id,
        data_file: data(&copy),
        temporary_data_file: format!(".{}.tmp", data(&copy)),
        manifest: receipt.manifest.clone(),
    });
    state.newest_receipt = None;
    state.observed_equal_at = None;
    state.receipts.clear();
    fixture.write(&state);
    // The source changes before the next push.
    fixture.change("second");

    let run = fixture.push();
    assert_eq!(run.report.recovered.as_deref(), Some(copy.as_str()));
    // The recovered copy was confirmed, not put again: its file is the same
    // file, untouched.
    assert_eq!(fs::read(directory.join(data(&copy))).unwrap(), stored);
    assert_eq!(
        fs::metadata(directory.join(data(&copy)))
            .unwrap()
            .modified()
            .unwrap(),
        modified
    );
    let after = fixture.state();
    assert_eq!(after.receipts[0].manifest, receipt.manifest);
    assert_eq!(after.receipts[0].target_identity, receipt.target_identity);
    // The changed store was then captured and put as a copy of its own.
    assert_eq!(run.report.outcome, Outcome::Uploaded, "{:?}", run.report);
    assert_eq!(after.receipts.len(), 2);
    assert_ne!(after.newest_receipt.unwrap().manifest.copy, copy);
    assert_eq!(after.pending, None);
}

#[test]
fn a_pending_attempt_that_never_arrived_is_dropped_with_only_its_own_files_removed() {
    let fixture = fixture();
    assert_eq!(fixture.push().report.outcome, Outcome::Uploaded);
    let mut state = fixture.state();
    let attempt = attempt_like(&state, uuid::Uuid::now_v7());
    let directory = fixture.project_dir(&fixture.copies);
    // A cut-off write left the attempt's temporary file; other files are not
    // this attempt's.
    fs::write(directory.join(&attempt.temporary_data_file), b"partial").unwrap();
    fs::write(directory.join("unrelated.txt"), b"keep me").unwrap();
    let foreign = format!("{}.db.gz", copy_name(Utc::now(), uuid::Uuid::now_v7()));
    fs::write(directory.join(&foreign), b"another home's copy").unwrap();
    state.pending = Some(attempt.clone());
    fixture.write(&state);
    let mut before = fixture.files(&fixture.copies);
    before.retain(|name| name != &attempt.temporary_data_file);

    let run = fixture.push();
    assert_eq!(
        run.report.dropped.as_deref(),
        Some(attempt.manifest.copy.as_str())
    );
    assert_eq!(run.report.outcome, Outcome::Unchanged, "{:?}", run.report);
    assert_eq!(fixture.files(&fixture.copies), before);
    assert_eq!(
        fs::read(directory.join("unrelated.txt")).unwrap(),
        b"keep me"
    );
    let after = fixture.state();
    assert_eq!(after.pending, None);
    assert_eq!(after.newest_receipt, state.newest_receipt);
}

#[test]
fn a_pending_attempt_for_another_target_is_set_aside_and_nothing_is_touched_for_it() {
    let fixture = fixture();
    assert_eq!(fixture.push().report.outcome, Outcome::Uploaded);
    let mut state = fixture.state();
    // An attempt made for an earlier target, whose temporary file lies at
    // the configured directory.
    let mut attempt = attempt_like(&state, uuid::Uuid::now_v7());
    attempt.manifest.target_identity = ObjectId::from_canonical_bytes(b"an earlier target");
    let directory = fixture.project_dir(&fixture.copies);
    fs::write(directory.join(&attempt.temporary_data_file), b"partial").unwrap();
    state.pending = Some(attempt.clone());
    fixture.write(&state);
    let before = fixture.files(&fixture.copies);

    let run = fixture.push();
    assert_eq!(
        run.report.set_aside.as_deref(),
        Some(attempt.manifest.copy.as_str())
    );
    assert_eq!(run.report.outcome, Outcome::Unchanged, "{:?}", run.report);
    let after = fixture.state();
    assert_eq!(after.pending, None);
    assert_eq!(after.set_aside, [attempt]);
    assert_eq!(fixture.files(&fixture.copies), before);
}

#[test]
fn a_newest_copy_the_target_lost_is_replaced() {
    let fixture = fixture();
    assert_eq!(fixture.push().report.outcome, Outcome::Uploaded);
    let lost = fixture.state().newest_receipt.unwrap();
    fs::remove_file(
        fixture
            .project_dir(&fixture.copies)
            .join(data(&lost.manifest.copy)),
    )
    .unwrap();

    let run = fixture.push();
    assert_eq!(run.report.outcome, Outcome::Uploaded, "{:?}", run.report);
    assert!(run.report.warnings[0].contains(&lost.manifest.copy));
    let newest = fixture.state().newest_receipt.unwrap();
    assert_ne!(newest.manifest.copy, lost.manifest.copy);
    assert_eq!(newest.sha256, lost.sha256);
}

#[test]
fn a_target_that_cannot_say_whether_it_holds_the_copy_fails_the_push_and_advances_no_time() {
    let fixture = fixture();
    assert_eq!(fixture.push().report.outcome, Outcome::Uploaded);
    let before = fixture.state();
    let away = fixture.home().join("copies-away");
    fs::rename(&fixture.copies, &away).unwrap();

    let run = fixture.push();
    assert_eq!(run.report.outcome, Outcome::Failed);
    assert_eq!(
        run.report.code.as_deref(),
        Some("backup_target_unconfirmed")
    );
    let after = fixture.state();
    assert_eq!(after.newest_receipt, before.newest_receipt);
    assert_eq!(after.observed_equal_at, before.observed_equal_at);
    let last = after.last_attempt.unwrap();
    assert_eq!(last.outcome, AttemptOutcome::Failed);
    assert_eq!(last.code.as_deref(), Some("backup_target_unconfirmed"));
    assert!(last.message.unwrap().contains("could not say"));
}

#[test]
fn a_changed_target_identity_gets_a_new_copy_of_an_unchanged_store() {
    let fixture = fixture();
    assert_eq!(fixture.push().report.outcome, Outcome::Uploaded);
    let earlier = fixture.state().newest_receipt.unwrap();
    // Another operator authorizes the same directory: a new identity.
    let view = fixture.set(&fixture.copies, "ann");
    assert_ne!(view.identity, earlier.target_identity);
    // Before a push the newest receipt names the earlier identity.
    assert_eq!(
        fixture.state().newest_receipt.unwrap().target_identity,
        earlier.target_identity
    );

    let run = fixture.push();
    assert_eq!(run.report.outcome, Outcome::Uploaded, "{:?}", run.report);
    let state = fixture.state();
    let newest = state.newest_receipt.clone().unwrap();
    // The same bytes, put again as a new copy for the current identity: the
    // newest receipt names the configured target, so the kind no longer
    // reads as changed.
    assert_eq!(newest.sha256, earlier.sha256);
    assert_ne!(newest.manifest.copy, earlier.manifest.copy);
    assert_eq!(newest.target_identity, view.identity);
    assert_eq!(newest.manifest.target_identity, view.identity);
    assert_eq!(state.receipts.len(), 2);
    assert_eq!(fixture.files(&fixture.copies).len(), 4);
    assert_eq!(fixture.push().report.outcome, Outcome::Unchanged);
}

#[test]
fn a_failed_push_keeps_the_previous_receipt_and_copy_and_records_the_attempt() {
    let fixture = fixture();
    assert_eq!(fixture.push().report.outcome, Outcome::Uploaded);
    let before = fixture.state();
    let copies_before = fixture.files(&fixture.copies);
    let check = |run: &PushRun, code: &str| {
        assert_eq!(run.report.outcome, Outcome::Failed, "{:?}", run.report);
        let report = serde_json::to_value(&run.report).unwrap();
        assert_eq!(report["outcome"], "failed");
        assert_eq!(report["code"], code, "{report}");
        assert!(
            report["message"]
                .as_str()
                .is_some_and(|message| !message.is_empty())
        );
        let after = fixture.state();
        assert_eq!(after.newest_receipt, before.newest_receipt);
        assert_eq!(after.observed_equal_at, before.observed_equal_at);
        assert_eq!(after.receipts, before.receipts);
        let last = after.last_attempt.unwrap();
        assert_eq!(last.outcome, AttemptOutcome::Failed);
        assert_eq!(last.code.as_deref(), Some(code));
        assert!(last.started_at <= last.ended_at);
        assert!(last.message.is_some());
        assert_eq!(fixture.files(&fixture.copies), copies_before);
        assert_eq!(fixture.stages(), Vec::<PathBuf>::new());
    };

    // The local stage has no room: nothing is captured.
    fixture.change("second");
    let mut cramped = settings();
    cramped.stage_free_space = |_| Ok(0);
    check(&fixture.push_with(&cramped), "backup_stage_no_space");
    assert_eq!(fixture.state().pending, None);

    // The configured directory is gone: the put is refused.
    let away = fixture.home().join("copies-away");
    fs::rename(&fixture.copies, &away).unwrap();
    let run = fixture.push();
    fs::rename(&away, &fixture.copies).unwrap();
    check(&run, "backup_target_unreachable");
    // The attempt stays pending for the next push to resolve.
    assert!(fixture.state().pending.is_some());

    // A target where this project's directory cannot be written, because a
    // file stands in its place.
    let blocked = fixture.home().join("blocked");
    fs::create_dir_all(&blocked).unwrap();
    fs::write(fixture.project_dir(&blocked), b"not a directory").unwrap();
    fixture.set(&blocked, "greg");
    let before_blocked = fixture.state();
    let run = fixture.push();
    assert_eq!(run.report.outcome, Outcome::Failed, "{:?}", run.report);
    assert_eq!(run.report.code.as_deref(), Some("backup_io"));
    let after = fixture.state();
    assert_eq!(after.newest_receipt, before_blocked.newest_receipt);
    assert_eq!(after.observed_equal_at, before_blocked.observed_equal_at);
    assert_eq!(
        after.last_attempt.unwrap().code.as_deref(),
        Some("backup_io")
    );
    assert_eq!(fixture.files(&fixture.copies), copies_before);
    assert_eq!(
        fs::read(fixture.project_dir(&blocked)).unwrap(),
        b"not a directory"
    );
}

#[test]
fn a_put_past_the_transport_deadline_stays_pending_and_the_next_push_confirms_it() {
    let fixture = fixture();
    let (release, held) = mpsc::channel::<()>();
    let held = Arc::new(Mutex::new(held));
    let mut hurried = settings();
    hurried.transport_deadline = Duration::from_millis(300);
    hurried.before_put = Some(Arc::new(move || {
        let _ = held.lock().unwrap().recv();
    }));

    let run = fixture.push_with(&hurried);
    assert_eq!(run.report.outcome, Outcome::Failed, "{:?}", run.report);
    assert_eq!(
        run.report.code.as_deref(),
        Some("backup_transport_deadline")
    );
    // The run still holds the lock, so the state is read from its file.
    let recorded: TargetState =
        serde_json::from_slice(&fs::read(&fixture.paths().state).unwrap()).unwrap();
    let pending = recorded.pending.unwrap();
    assert_eq!(
        run.report.pending.as_deref(),
        Some(pending.manifest.copy.as_str())
    );
    assert_eq!(recorded.newest_receipt, None);
    assert_eq!(
        recorded.last_attempt.unwrap().code.as_deref(),
        Some("backup_transport_deadline")
    );
    // The worker is still there; the target completes the copy after the
    // deadline.
    let abandoned = run.abandoned.expect("the worker passed its deadline");
    // The push lock stays held while the worker may still be writing, so no
    // other push or target word acts on the target meanwhile.
    let error = PushLock::try_acquire(&fixture.paths()).unwrap_err();
    assert_eq!(error.code(), "backup_push_running", "{error}");
    assert_eq!(fixture.push().report.outcome, Outcome::Busy);
    release.send(()).unwrap();
    abandoned.worker.join().unwrap();
    drop(abandoned.lock);
    assert_eq!(
        fixture.files(&fixture.copies),
        [
            data(&pending.manifest.copy),
            manifest(&pending.manifest.copy)
        ]
    );

    let next = fixture.push();
    assert_eq!(
        next.report.recovered.as_deref(),
        Some(pending.manifest.copy.as_str())
    );
    assert_eq!(next.report.outcome, Outcome::Unchanged, "{:?}", next.report);
    let state = fixture.state();
    assert_eq!(state.newest_receipt.unwrap().manifest, pending.manifest);
    assert_eq!(state.pending, None);
    // The stage the abandoned worker read from was removed by this push.
    assert_eq!(fixture.stages(), Vec::<PathBuf>::new());
}

#[test]
fn a_capture_past_its_own_deadline_fails_without_any_request_to_the_target() {
    let fixture = fixture();
    let mut hurried = settings();
    hurried.capture_deadline = Duration::ZERO;
    let run = fixture.push_with(&hurried);
    assert_eq!(run.report.outcome, Outcome::Failed);
    assert_eq!(run.report.code.as_deref(), Some("backup_capture_deadline"));
    assert!(run.abandoned.is_none());
    assert_eq!(fixture.files(&fixture.copies), Vec::<String>::new());
    assert_eq!(fixture.state().pending, None);
}

#[test]
fn nothing_configured_and_a_held_lock_change_nothing() {
    let home = temp_home().unwrap();
    let project = ProjectId("push-unconfigured".into());
    let run = push(home.path(), &project, CopyKind::Store, &settings());
    assert_eq!(run.report.outcome, Outcome::NotConfigured);
    assert!(!home.path().join("backup-records").exists());

    let fixture = fixture();
    let lock = PushLock::try_acquire(&fixture.paths()).unwrap();
    // The lock is per open file, so this process's second attempt meets it.
    let run = fixture.push();
    assert_eq!(run.report.outcome, Outcome::Busy);
    drop(lock);
    assert_eq!(fixture.stages(), Vec::<PathBuf>::new());
    assert_eq!(fixture.files(&fixture.copies), Vec::<String>::new());
}

#[test]
fn a_failed_write_after_the_put_reports_what_is_recorded_and_the_next_push_recovers() {
    let fixture = fixture();
    let mut failing = settings();
    failing.fail_save_after_put = true;
    let run = fixture.push_with(&failing);
    assert_eq!(run.report.outcome, Outcome::Failed, "{:?}", run.report);
    assert_eq!(run.report.code.as_deref(), Some("backup_io"));
    // The report shows the recorded state: no receipt, no time, and the
    // attempt still pending, although the copy reached the target.
    let state = fixture.state();
    assert_eq!(state.newest_receipt, None);
    assert_eq!(state.observed_equal_at, None);
    let pending = state.pending.clone().expect("the attempt stays pending");
    assert_eq!(run.report.receipt, None);
    assert_eq!(run.report.observed_equal_at, None);
    assert_eq!(
        run.report.pending.as_deref(),
        Some(pending.manifest.copy.as_str())
    );
    let report = serde_json::to_value(&run.report).unwrap();
    assert_eq!(report["receipt"], serde_json::Value::Null);
    assert_eq!(report["pending"], pending.manifest.copy.as_str());
    assert_eq!(
        fixture.files(&fixture.copies),
        [
            data(&pending.manifest.copy),
            manifest(&pending.manifest.copy)
        ]
    );

    let next = fixture.push();
    assert_eq!(
        next.report.recovered.as_deref(),
        Some(pending.manifest.copy.as_str())
    );
    assert_eq!(next.report.outcome, Outcome::Unchanged, "{:?}", next.report);
}

#[test]
fn time_spent_asking_the_target_does_not_count_against_the_capture_deadline() {
    let fixture = fixture();
    assert_eq!(fixture.push().report.outcome, Outcome::Uploaded);
    let lost = fixture.state().newest_receipt.unwrap();
    fs::remove_file(
        fixture
            .project_dir(&fixture.copies)
            .join(data(&lost.manifest.copy)),
    )
    .unwrap();
    // The confirmation that finds the copy missing takes longer than the
    // whole capture deadline; the local capture and preparation do not.
    let mut slow = settings();
    slow.capture_deadline = Duration::from_secs(3);
    slow.after_confirm = Some(Arc::new(|| {
        std::thread::sleep(Duration::from_millis(3_500));
    }));
    let run = fixture.push_with(&slow);
    assert_eq!(run.report.outcome, Outcome::Uploaded, "{:?}", run.report);
    assert_ne!(
        fixture.state().newest_receipt.unwrap().manifest.copy,
        lost.manifest.copy
    );
}

#[test]
fn the_stage_sweep_removes_only_stages_a_push_wrote_and_follows_no_link() {
    let fixture = fixture();
    let root = fixture
        .home()
        .join(engram::backup::STAGE_DIRECTORY)
        .join(engram::project_digest(&fixture.project));
    fs::create_dir_all(&root).unwrap();
    // A stage a push left: its staged copy and stored file.
    let leftover = root.join(uuid::Uuid::now_v7().to_string());
    fs::create_dir(&leftover).unwrap();
    fs::write(leftover.join("store.db"), b"staged").unwrap();
    let copy = copy_name(Utc::now(), uuid::Uuid::now_v7());
    fs::write(leftover.join(data(&copy)), b"stored").unwrap();
    // A directory that is not a stage, holding a file a push would write.
    let other = root.join("not-a-stage");
    fs::create_dir(&other).unwrap();
    fs::write(other.join("store.db"), b"keep").unwrap();
    // A link named like a stage, leading to unrelated files.
    let outside = fixture.home().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("store.db"), b"unrelated").unwrap();
    fs::write(outside.join(data(&copy)), b"unrelated").unwrap();
    let link = root.join(uuid::Uuid::now_v7().to_string());
    make_dir_link(&outside, &link);

    let run = fixture.push();
    assert_eq!(run.report.outcome, Outcome::Uploaded, "{:?}", run.report);
    assert!(!leftover.exists());
    assert_eq!(fs::read(other.join("store.db")).unwrap(), b"keep");
    assert_eq!(fs::read(outside.join("store.db")).unwrap(), b"unrelated");
    assert_eq!(fs::read(outside.join(data(&copy))).unwrap(), b"unrelated");
    for kept in [&other, &link] {
        assert!(
            run.report
                .warnings
                .iter()
                .any(|warning| warning.contains(&kept.display().to_string())),
            "{:?}",
            run.report.warnings
        );
    }
    remove_dir_link(&link);
}

#[test]
fn a_confirmed_recovery_stands_when_the_capture_after_it_fails() {
    let fixture = fixture();
    assert_eq!(fixture.push().report.outcome, Outcome::Uploaded);
    let mut state = fixture.state();
    let receipt = state.newest_receipt.clone().unwrap();
    let copy = receipt.manifest.copy.clone();
    let id = uuid::Uuid::parse_str(copy.split_once('-').unwrap().1).unwrap();
    state.pending = Some(Attempt {
        id,
        data_file: data(&copy),
        temporary_data_file: format!(".{}.tmp", data(&copy)),
        manifest: receipt.manifest.clone(),
    });
    state.newest_receipt = None;
    state.observed_equal_at = None;
    state.receipts.clear();
    fixture.write(&state);

    let mut cramped = settings();
    cramped.stage_free_space = |_| Ok(0);
    let run = fixture.push_with(&cramped);
    assert_eq!(run.report.outcome, Outcome::Failed);
    assert_eq!(run.report.code.as_deref(), Some("backup_stage_no_space"));
    assert_eq!(run.report.recovered.as_deref(), Some(copy.as_str()));
    let after = fixture.state();
    assert_eq!(after.newest_receipt.unwrap().manifest, receipt.manifest);
    assert_eq!(after.pending, None);
    assert_eq!(
        after.observed_equal_at,
        Some(receipt.manifest.capture.capture_started_at)
    );
}
