#[path = "../src/test_support.rs"]
mod test_support;

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use engram::{
    ProjectId,
    backup::{
        CopyKind,
        target::{PushLock, RecordPaths},
    },
};
use serde_json::Value;

const PROJECT: &str = "push-cli-fixture";

fn engram(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_engram"))
        .env_remove("ENGRAM_HOME")
        .arg("--home")
        .arg(root.join("home"))
        .arg("--project-file")
        .arg(root.join(".engram-project"))
        .args(args)
        .output()
        .expect("run engram")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// An initialized store for the fixture project.
fn setup(root: &Path) {
    fs::write(root.join(".engram-project"), format!("{PROJECT}\n")).unwrap();
    let init = engram(root, &["init"]);
    assert!(init.status.success(), "{}", text(&init.stderr));
}

fn set_target(root: &Path, dir: &Path) {
    let set = engram(
        root,
        &[
            "backup",
            "target",
            "set",
            "--kind",
            "store",
            "--adapter",
            "directory",
            "--dir",
            dir.to_str().unwrap(),
            "--disclosure-authorized-by",
            "greg",
            "--off-host-asserted-by",
            "greg",
        ],
    );
    assert!(set.status.success(), "{}", text(&set.stderr));
}

fn project() -> ProjectId {
    ProjectId(PROJECT.into())
}

/// The fixture project's directory at the target `root`.
fn project_dir(root: &Path) -> PathBuf {
    root.join(engram::project_digest(&project()))
}

/// Runs `backup push --json`, checks its exit code and returns its one kind.
fn push_json(root: &Path, success: bool) -> Value {
    let push = engram(root, &["backup", "push", "--json"]);
    assert_eq!(
        push.status.success(),
        success,
        "{}{}",
        text(&push.stdout),
        text(&push.stderr)
    );
    let value: Value = serde_json::from_slice(&push.stdout).unwrap();
    assert_eq!(value["project"], PROJECT);
    let kinds = value["kinds"].as_array().unwrap();
    assert_eq!(kinds.len(), 1, "{value}");
    assert_eq!(kinds[0]["kind"], "store");
    kinds[0].clone()
}

#[test]
fn push_with_no_target_configured_says_so_and_exits_zero() {
    let root = test_support::temp_home().unwrap();
    setup(root.path());
    let push = engram(root.path(), &["backup", "push"]);
    assert!(push.status.success(), "{}", text(&push.stderr));
    assert_eq!(
        text(&push.stdout).trim(),
        "No store backup target is configured for this project."
    );
    assert_eq!(push_json(root.path(), true)["outcome"], "not_configured");
    let home = root.path().join("home");
    assert!(!home.join("backup-records").exists());
    assert!(!home.join("backup-stage").exists());
}

#[test]
fn push_puts_a_copy_and_records_its_receipt_with_its_manifest() {
    let root = test_support::temp_home().unwrap();
    setup(root.path());
    let copies = root.path().join("copies");
    fs::create_dir_all(&copies).unwrap();
    set_target(root.path(), &copies);

    let report = push_json(root.path(), true);
    assert_eq!(report["outcome"], "uploaded", "{report}");
    assert_eq!(report["code"], Value::Null);
    let copy = report["receipt"]["manifest"]["copy"].as_str().unwrap();
    let sha256 = report["receipt"]["sha256"].as_str().unwrap();
    let directory = project_dir(&copies);
    assert!(directory.join(format!("{copy}.db.gz")).is_file());
    assert!(directory.join(format!("{copy}.manifest.json")).is_file());

    // The state file records the receipt with its manifest.
    let paths = RecordPaths::new(&root.path().join("home"), &project(), CopyKind::Store);
    let state: Value = serde_json::from_slice(&fs::read(&paths.state).unwrap()).unwrap();
    assert_eq!(state["newest_receipt"]["manifest"]["copy"], copy);
    assert_eq!(state["newest_receipt"]["sha256"], sha256);
    assert_eq!(
        state["newest_receipt"]["manifest"]["capture"]["sha256"],
        sha256
    );
    assert_eq!(state["pending"], Value::Null);
    assert_eq!(state["last_attempt"]["outcome"], "uploaded");

    // The same store again: the copy is confirmed, not put again.
    let push = engram(root.path(), &["backup", "push"]);
    assert!(push.status.success(), "{}", text(&push.stderr));
    assert_eq!(
        text(&push.stdout).trim(),
        format!("store: unchanged; the newest copy was confirmed: {copy}")
    );
}

#[test]
fn push_while_another_holds_the_lock_exits_zero_without_capturing() {
    let root = test_support::temp_home().unwrap();
    setup(root.path());
    let copies = root.path().join("copies");
    fs::create_dir_all(&copies).unwrap();
    set_target(root.path(), &copies);
    let home = root.path().join("home");
    let lock =
        PushLock::try_acquire(&RecordPaths::new(&home, &project(), CopyKind::Store)).unwrap();

    let report = push_json(root.path(), true);
    assert_eq!(report["outcome"], "busy", "{report}");
    let push = engram(root.path(), &["backup", "push"]);
    assert!(push.status.success(), "{}", text(&push.stderr));
    assert_eq!(
        text(&push.stdout).trim(),
        "store: another push is running; nothing was done."
    );
    // Nothing was captured or put.
    assert!(!home.join("backup-stage").exists());
    assert!(!project_dir(&copies).exists());
    drop(lock);
}

#[test]
fn a_failed_push_exits_one_with_its_typed_code() {
    let root = test_support::temp_home().unwrap();
    setup(root.path());
    // The configured directory does not exist: it is never created.
    let missing = root.path().join("missing-copies");
    set_target(root.path(), &missing);

    let report = push_json(root.path(), false);
    assert_eq!(report["outcome"], "failed", "{report}");
    assert_eq!(report["code"], "backup_target_unreachable");
    assert!(
        report["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty())
    );
    assert!(report["pending"].is_string(), "{report}");
    assert!(!missing.exists());

    // The next push first resolves the attempt left pending, which the
    // unreachable target cannot answer for either.
    let push = engram(root.path(), &["backup", "push"]);
    assert_eq!(push.status.code(), Some(1));
    let printed = text(&push.stdout);
    assert!(
        printed.starts_with("store: push failed: backup_pending_unresolved: "),
        "{printed}"
    );
    assert!(printed.contains("attempt left pending: "), "{printed}");
}

#[test]
fn deadlines_too_far_off_for_the_clock_push_without_a_limit() {
    let root = test_support::temp_home().unwrap();
    setup(root.path());
    let copies = root.path().join("copies");
    fs::create_dir_all(&copies).unwrap();
    set_target(root.path(), &copies);
    let max = u64::MAX.to_string();

    let push = engram(
        root.path(),
        &[
            "backup",
            "push",
            "--json",
            "--capture-deadline-secs",
            &max,
            "--transport-deadline-secs",
            &max,
        ],
    );
    assert!(
        push.status.success(),
        "{}{}",
        text(&push.stdout),
        text(&push.stderr)
    );
    let value: Value = serde_json::from_slice(&push.stdout).unwrap();
    assert_eq!(value["kinds"][0]["outcome"], "uploaded", "{value}");
    let stage = root
        .path()
        .join("home")
        .join(engram::backup::STAGE_DIRECTORY)
        .join(engram::project_digest(&project()));
    let left: Vec<_> = match fs::read_dir(&stage) {
        Ok(entries) => entries.map(|entry| entry.unwrap().path()).collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("{error}"),
    };
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn a_transport_deadline_shorter_than_the_put_exits_one_and_leaves_the_attempt_pending() {
    let root = test_support::temp_home().unwrap();
    setup(root.path());
    let copies = root.path().join("copies");
    fs::create_dir_all(&copies).unwrap();
    set_target(root.path(), &copies);

    let push = engram(
        root.path(),
        &["backup", "push", "--json", "--transport-deadline-secs", "0"],
    );
    assert_eq!(push.status.code(), Some(1), "{}", text(&push.stderr));
    let value: Value = serde_json::from_slice(&push.stdout).unwrap();
    let report = &value["kinds"][0];
    assert_eq!(report["outcome"], "failed", "{report}");
    assert_eq!(report["code"], "backup_transport_deadline");
    let pending = report["pending"].as_str().unwrap();
    let paths = RecordPaths::new(&root.path().join("home"), &project(), CopyKind::Store);
    let state: Value = serde_json::from_slice(&fs::read(&paths.state).unwrap()).unwrap();
    assert_eq!(state["pending"]["manifest"]["copy"], pending);
    assert_eq!(state["newest_receipt"], Value::Null);
    assert_eq!(state["last_attempt"]["code"], "backup_transport_deadline");

    // With time to put it, the next push resolves the attempt, which never
    // arrived, and puts the copy.
    let report = push_json(root.path(), true);
    assert_eq!(report["dropped"], pending, "{report}");
    assert_eq!(report["outcome"], "uploaded");
}
