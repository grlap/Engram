//! Pushes killed at named stages. Each push runs in a process of its own,
//! which stops at its stage and is killed there, so it leaves on disk exactly
//! what a host that terminates a push at that point leaves. The next push
//! then runs on the same home and target.

use std::{
    fs,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use engram::{
    ProjectId,
    backup::{
        CopyKind,
        target::{PushLock, TargetState},
    },
};

use super::{Fixture, attempt_like, data, fixture, manifest, settings};
use crate::bin_support::backup::{
    halt::{self, Stage},
    push::{Outcome, PushRun, push},
};

const HOME_VARIABLE: &str = "ENGRAM_KILLED_PUSH_HOME";
const STAGE_VARIABLE: &str = "ENGRAM_KILLED_PUSH_STAGE";
const READY: &str = "killed-push-ready";

/// The push that a test below runs in a child process and kills: it stops at
/// its stage and waits there. It runs only in that exact filtered child.
#[test]
fn killed_push_process() {
    let (Some(home), Some(stage)) = (
        std::env::var_os(HOME_VARIABLE),
        std::env::var(STAGE_VARIABLE).ok(),
    ) else {
        return;
    };
    let home = PathBuf::from(home);
    let stage = Stage::named(&stage).expect("a known stage");
    halt::arm(stage, home.join(READY));
    let run = push(
        &home,
        &ProjectId("push-project".into()),
        CopyKind::Store,
        &settings(),
    );
    panic!(
        "the push ended before it reached {stage:?}: {:?}",
        run.report
    );
}

/// Kills and reaps the child, whatever the test's outcome.
struct Killed(Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Runs a push of `fixture` in a child process, waits until it stops at
/// `stage`, and kills it there.
fn kill_at(fixture: &Fixture, stage: Stage) {
    let ready = fixture.home().join(READY);
    let mut child = Killed(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "bin_support::backup::push::tests::killed::killed_push_process",
                "--nocapture",
            ])
            .env(HOME_VARIABLE, fixture.home())
            .env(STAGE_VARIABLE, stage.name())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let give_up = Instant::now() + Duration::from_secs(60);
    while !ready.exists() {
        if let Some(status) = child.0.try_wait().unwrap() {
            panic!("the push ended before it stopped at {stage:?}: {status}");
        }
        assert!(
            Instant::now() < give_up,
            "the push never stopped at {stage:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    fs::remove_file(&ready).unwrap();
    // The system releases a killed holder's lock, though not necessarily by
    // the time the wait returns; what the kill left is read only once it has.
    let give_up = Instant::now() + Duration::from_secs(30);
    loop {
        match PushLock::try_acquire(&fixture.paths()) {
            Ok(_) => return,
            Err(error) => {
                assert_eq!(error.code(), "backup_push_running", "{error}");
                assert!(
                    Instant::now() < give_up,
                    "the killed push's lock was never released"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// The next push, which runs once the killed push's lock is free.
fn next_push(fixture: &Fixture) -> PushRun {
    let run = fixture.push();
    assert_ne!(run.report.outcome, Outcome::Busy, "{:?}", run.report);
    run
}

/// The files in each directory left in the project's stage, sorted.
fn staged_files(fixture: &Fixture) -> Vec<Vec<String>> {
    fixture
        .stages()
        .iter()
        .map(|stage| {
            let mut names: Vec<_> = fs::read_dir(stage)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        })
        .collect()
}

/// The files of the copy named `copy` at the target.
fn copy_files(copy: &str) -> [String; 2] {
    [data(copy), manifest(copy)]
}

fn sorted(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names
}

/// A store pushed once and then changed, so the killed push has a new copy
/// to put; its first copy and the state that push recorded.
fn pushed_then_changed(fixture: &Fixture) -> (String, TargetState) {
    let first = fixture.push();
    assert_eq!(
        first.report.outcome,
        Outcome::Uploaded,
        "{:?}",
        first.report
    );
    let state = fixture.state();
    let copy = state.newest_receipt.as_ref().unwrap().manifest.copy.clone();
    fixture.change("second");
    (copy, state)
}

/// The one stage a killed push left, holding the staged copy and the
/// compressed file of its attempt `copy`.
fn assert_stage_with_its_copy(fixture: &Fixture, copy: &str) {
    let left = staged_files(fixture);
    assert_eq!(left.len(), 1, "{left:?}");
    assert!(left[0].iter().any(|name| name == "store.db"), "{left:?}");
    assert!(left[0].contains(&data(copy)), "{left:?}");
}

/// What every next push leaves, whatever the stage of the kill: no stage, no
/// pending attempt and no temporary record file.
fn assert_settled(fixture: &Fixture) {
    assert!(fixture.stages().is_empty(), "{:?}", staged_files(fixture));
    assert!(fixture.state().pending.is_none());
    let records: Vec<_> = fs::read_dir(&fixture.paths().directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| {
            std::path::Path::new(name)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("tmp"))
        })
        .collect();
    assert!(records.is_empty(), "{records:?}");
}

/// A kill before or during the capture leaves the records and the target as
/// they were; only a stage may be left, which the next push removes before
/// it uploads the changed store.
fn killed_before_any_attempt(stage: Stage, staged: &[&str]) {
    let fixture = fixture();
    let (first, before) = pushed_then_changed(&fixture);
    kill_at(&fixture, stage);

    assert_eq!(fixture.state(), before);
    assert_eq!(
        fixture.files(&fixture.copies),
        sorted(copy_files(&first).into())
    );
    let left = staged_files(&fixture);
    if staged.is_empty() {
        assert!(left.is_empty(), "{left:?}");
    } else {
        assert_eq!(left.len(), 1, "{left:?}");
        for name in staged {
            assert!(
                left[0].iter().any(|file| file.ends_with(name)),
                "{name} in {left:?}"
            );
        }
    }

    let next = next_push(&fixture);
    assert_eq!(next.report.outcome, Outcome::Uploaded, "{:?}", next.report);
    assert!(next.report.dropped.is_none() && next.report.recovered.is_none());
    assert_settled(&fixture);
}

#[test]
fn a_push_killed_before_its_capture_leaves_everything_as_it_was() {
    killed_before_any_attempt(Stage::BeforeCapture, &[]);
}

#[test]
fn a_push_killed_while_resolving_an_attempt_leaves_it_pending_for_the_next_push() {
    let fixture = fixture();
    assert_eq!(fixture.push().report.outcome, Outcome::Uploaded);
    let mut state = fixture.state();
    // An earlier push recorded this attempt and never got its copy there.
    let attempt = attempt_like(&state, uuid::Uuid::now_v7());
    state.pending = Some(attempt.clone());
    fixture.write(&state);
    kill_at(&fixture, Stage::Resolving);

    // The target answered, but nothing of the answer was recorded.
    assert_eq!(fixture.state(), state);
    assert!(fixture.stages().is_empty(), "{:?}", staged_files(&fixture));

    let next = next_push(&fixture);
    assert_eq!(
        next.report.dropped.as_deref(),
        Some(attempt.manifest.copy.as_str())
    );
    assert_eq!(next.report.outcome, Outcome::Unchanged, "{:?}", next.report);
    assert_settled(&fixture);
}

#[test]
fn a_push_killed_after_its_copy_leaves_only_a_stage_the_next_push_removes() {
    killed_before_any_attempt(Stage::CopyStaged, &["store.db"]);
}

#[test]
fn a_push_killed_after_preparing_its_file_leaves_only_a_stage_the_next_push_removes() {
    killed_before_any_attempt(Stage::Prepared, &["store.db", ".db.gz"]);
}

/// A kill after the attempt is recorded and before the target holds its
/// whole copy leaves the attempt pending; the next push finds it missing,
/// drops it with any file of it at the target, and uploads anew.
fn killed_before_the_copy_arrived(stage: Stage, at_target: impl Fn(&str) -> Vec<String>) {
    let fixture = fixture();
    let (first, before) = pushed_then_changed(&fixture);
    kill_at(&fixture, stage);

    let killed = fixture.state();
    let pending = killed.pending.as_ref().expect("the attempt stays pending");
    let attempt = pending.manifest.copy.clone();
    assert_eq!(killed.last_attempt, before.last_attempt);
    assert_eq!(killed.newest_receipt, before.newest_receipt);
    assert_stage_with_its_copy(&fixture, &attempt);
    let mut expected: Vec<String> = copy_files(&first).into();
    expected.extend(at_target(&attempt));
    assert_eq!(fixture.files(&fixture.copies), sorted(expected));

    let next = next_push(&fixture);
    assert_eq!(next.report.outcome, Outcome::Uploaded, "{:?}", next.report);
    assert_eq!(next.report.dropped.as_deref(), Some(attempt.as_str()));
    let newest = fixture.state().newest_receipt.unwrap().manifest.copy;
    let mut expected: Vec<String> = copy_files(&first).into();
    expected.extend(copy_files(&newest));
    assert_eq!(fixture.files(&fixture.copies), sorted(expected));
    assert_settled(&fixture);
}

#[test]
fn a_push_killed_after_recording_its_attempt_leaves_it_pending_and_the_next_push_drops_it() {
    killed_before_the_copy_arrived(Stage::PendingRecorded, |_| Vec::new());
}

#[test]
fn a_push_killed_while_writing_to_the_target_leaves_a_temporary_file_the_next_push_removes() {
    killed_before_the_copy_arrived(Stage::TemporaryWritten, |copy| {
        vec![format!(".{}.tmp", data(copy))]
    });
}

/// A kill once the copy's data is in place at the target leaves the attempt
/// pending; the next push finishes or finds the copy, records it as the
/// newest, and finds the store unchanged since.
fn killed_after_the_copy_arrived(stage: Stage, at_target: impl Fn(&str) -> Vec<String>) {
    let fixture = fixture();
    let (first, before) = pushed_then_changed(&fixture);
    kill_at(&fixture, stage);

    let killed = fixture.state();
    let attempt = killed.pending.as_ref().unwrap().manifest.copy.clone();
    assert_eq!(killed.last_attempt, before.last_attempt);
    assert_eq!(killed.newest_receipt, before.newest_receipt);
    assert_stage_with_its_copy(&fixture, &attempt);
    let mut expected: Vec<String> = copy_files(&first).into();
    expected.extend(at_target(&attempt));
    assert_eq!(fixture.files(&fixture.copies), sorted(expected));

    let next = next_push(&fixture);
    assert_eq!(next.report.outcome, Outcome::Unchanged, "{:?}", next.report);
    assert_eq!(next.report.recovered.as_deref(), Some(attempt.as_str()));
    let settled = fixture.state();
    assert_eq!(settled.newest_receipt.unwrap().manifest.copy, attempt);
    let mut expected: Vec<String> = copy_files(&first).into();
    expected.extend(copy_files(&attempt));
    assert_eq!(fixture.files(&fixture.copies), sorted(expected));
    assert_settled(&fixture);
}

#[test]
fn a_push_killed_before_writing_its_manifest_leaves_a_copy_the_next_push_completes() {
    killed_after_the_copy_arrived(Stage::DataPublished, |copy| vec![data(copy)]);
}

#[test]
fn a_push_killed_before_recording_its_receipt_leaves_a_copy_the_next_push_confirms() {
    killed_after_the_copy_arrived(Stage::PutReturned, |copy| copy_files(copy).into());
}

#[test]
fn a_push_killed_during_retention_keeps_its_receipt_and_the_next_push_finishes_the_removal() {
    let fixture = fixture();
    fixture.set_keeping(&fixture.copies, "greg", 1);
    let (first, _) = pushed_then_changed(&fixture);
    kill_at(&fixture, Stage::CopyRemoved);

    // The receipt and the last attempt were recorded before the removal; the
    // removed copy is still in the ledger.
    let state = fixture.state();
    let newest = state.newest_receipt.as_ref().unwrap().manifest.copy.clone();
    assert_ne!(newest, first);
    assert!(state.pending.is_none());
    assert_eq!(
        state.last_attempt.as_ref().map(|attempt| attempt.outcome),
        Some(engram::backup::record::AttemptOutcome::Uploaded)
    );
    let ledger: Vec<_> = state
        .receipts
        .iter()
        .map(|receipt| receipt.manifest.copy.clone())
        .collect();
    assert_eq!(ledger, [first.clone(), newest.clone()]);
    assert_eq!(
        fixture.files(&fixture.copies),
        sorted(copy_files(&newest).into())
    );

    let next = next_push(&fixture);
    assert_eq!(next.report.outcome, Outcome::Unchanged, "{:?}", next.report);
    assert_eq!(next.report.removed, [first]);
    let ledger: Vec<_> = fixture
        .state()
        .receipts
        .iter()
        .map(|receipt| receipt.manifest.copy.clone())
        .collect();
    assert_eq!(ledger, std::slice::from_ref(&newest));
    assert_eq!(
        fixture.files(&fixture.copies),
        sorted(copy_files(&newest).into())
    );
    assert_settled(&fixture);
}

#[test]
fn a_push_removes_the_temporary_record_files_a_killed_write_left() {
    let fixture = fixture();
    let directory = fixture.paths().directory;
    let id = uuid::Uuid::now_v7();
    let leftovers = [
        format!("store.state.json.{id}.tmp"),
        format!("store.target.json.{id}.tmp"),
        format!("store.restore.json.{id}.tmp"),
    ];
    for name in &leftovers {
        fs::write(directory.join(name), b"{").unwrap();
    }
    // Another id for the name in capitals: on a file system that ignores
    // case it would otherwise be the leftover's own file.
    let kept = [
        format!(
            "store.state.json.{}.tmp",
            uuid::Uuid::now_v7().hyphenated().to_string().to_uppercase()
        ),
        "store.state.json.not-an-id.tmp".to_owned(),
        format!("store.history.json.{id}.tmp"),
    ];
    for name in &kept {
        fs::write(directory.join(name), b"{").unwrap();
    }
    let kept_directory = directory.join(format!("store.state.json.{}.tmp", uuid::Uuid::now_v7()));
    fs::create_dir(&kept_directory).unwrap();

    let run = fixture.push();
    assert_eq!(run.report.outcome, Outcome::Uploaded, "{:?}", run.report);
    for name in &leftovers {
        assert!(!directory.join(name).exists(), "{name}");
    }
    for name in &kept {
        assert!(directory.join(name).is_file(), "{name}");
    }
    assert!(kept_directory.is_dir());
    assert!(run.report.warnings.is_empty(), "{:?}", run.report.warnings);
}
