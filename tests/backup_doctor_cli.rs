//! `engram doctor` carries the backup block that `engram backup status`
//! reports, beside and apart from the store's health, and contacts no
//! target; `engram readiness` carries nothing about backups.

#[path = "../src/test_support.rs"]
mod test_support;

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use engram::{ProjectId, project_database_path};
use serde_json::Value;

const PROJECT: &str = "doctor-fixture";

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

fn succeeded(root: &Path, args: &[&str]) -> Output {
    let output = engram(root, args);
    assert!(
        output.status.success(),
        "{args:?}: {}{}",
        text(&output.stdout),
        text(&output.stderr)
    );
    output
}

fn setup(root: &Path) -> PathBuf {
    fs::write(root.join(".engram-project"), format!("{PROJECT}\n")).unwrap();
    succeeded(root, &["init"]);
    project_database_path(&root.join("home"), &ProjectId(PROJECT.into()))
}

fn set_target(root: &Path, dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    succeeded(
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
}

/// The status without what changes with the clock alone.
fn timeless(mut value: Value) -> Value {
    if let Some(fields) = value.as_object_mut() {
        fields.remove("as_of");
    }
    for kind in value["kinds"].as_array_mut().into_iter().flatten() {
        if let Some(copy) = kind["target"]["copy"].as_object_mut() {
            copy.remove("capture_age_seconds");
        }
    }
    value
}

fn timeless_text(block: &str) -> Vec<String> {
    block
        .lines()
        .filter(|line| !line.contains("(age "))
        .map(str::to_owned)
        .collect()
}

/// Doctor's JSON report and text, which must both succeed.
fn doctor(root: &Path) -> (Value, String) {
    let json = succeeded(root, &["doctor", "--json"]);
    let value: Value = serde_json::from_slice(&json.stdout).unwrap();
    let plain = succeeded(root, &["doctor"]);
    (value, text(&plain.stdout))
}

/// `backup status` in JSON and text.
fn status(root: &Path) -> (Value, String) {
    let json = succeeded(root, &["backup", "status", "--json"]);
    let value: Value = serde_json::from_slice(&json.stdout).unwrap();
    let plain = succeeded(root, &["backup", "status"]);
    (value, text(&plain.stdout))
}

/// Doctor's block, in JSON and in text, is exactly what `backup status`
/// reports at the same moment.
fn assert_doctor_block_is_status(root: &Path) -> Value {
    let (report, plain) = doctor(root);
    let (status, status_text) = status(root);
    assert_eq!(timeless(report["backup"].clone()), timeless(status.clone()));
    let block = timeless_text(&status_text);
    let printed = timeless_text(&plain);
    assert!(
        printed.windows(block.len()).any(|lines| lines == block),
        "doctor text:\n{plain}\nstatus text:\n{status_text}"
    );
    report
}

#[test]
fn doctor_prints_the_backup_status_block_in_text_and_json() {
    let root = test_support::temp_home().unwrap();
    setup(root.path());
    // No target: the block says so, and the store is healthy.
    let report = assert_doctor_block_is_status(root.path());
    assert_eq!(report["healthy"], true);
    assert_eq!(report["backup"]["durability"]["mode"], "local");
    assert_eq!(
        report["backup"]["kinds"][0]["reason"],
        "backup_not_configured"
    );

    set_target(root.path(), &root.path().join("copies"));
    succeeded(root.path(), &["backup", "push"]);
    let report = assert_doctor_block_is_status(root.path());
    assert_eq!(report["healthy"], true);
    assert_eq!(report["backup"]["durability"]["mode"], "local_backed_up");
    assert_eq!(report["backup"]["schema_version"], 1);
}

#[test]
fn doctor_contacts_no_target_and_its_block_and_exit_stay_unchanged() {
    let root = test_support::temp_home().unwrap();
    setup(root.path());
    let copies = root.path().join("copies");
    set_target(root.path(), &copies);
    succeeded(root.path(), &["backup", "push"]);
    let (before, _) = doctor(root.path());

    // The target directory is gone, so the directory adapter fails every
    // request it would make. Doctor's block and exit are unchanged, and
    // nothing re-creates the directory.
    let away = root.path().join("copies-away");
    fs::rename(&copies, &away).unwrap();
    let (after, _) = doctor(root.path());
    assert_eq!(
        timeless(after["backup"].clone()),
        timeless(before["backup"].clone())
    );
    assert_eq!(after["healthy"], true);
    assert!(!copies.exists());
    // The same world tells a command that does contact the target apart:
    // its check finds the target unreachable.
    let checked = succeeded(
        root.path(),
        &["backup", "status", "--check-target", "--json"],
    );
    let checked: Value = serde_json::from_slice(&checked.stdout).unwrap();
    assert_eq!(checked["checks"][0]["code"], "backup_target_unreachable");

    // The target is back but the copy is gone. Doctor still reads only the
    // recorded evidence, so its block is unchanged; a check that contacts
    // the target records the missing copy, and only then does doctor's
    // block change.
    fs::rename(&away, &copies).unwrap();
    let digest = engram::project_digest(&ProjectId(PROJECT.into()));
    for entry in fs::read_dir(copies.join(digest)).unwrap() {
        let path = entry.unwrap().path();
        if path.to_string_lossy().ends_with(".db.gz") {
            fs::remove_file(path).unwrap();
        }
    }
    let (unchanged, _) = doctor(root.path());
    assert_eq!(
        timeless(unchanged["backup"].clone()),
        timeless(before["backup"].clone())
    );
    let checked = succeeded(
        root.path(),
        &["backup", "status", "--check-target", "--json"],
    );
    let checked: Value = serde_json::from_slice(&checked.stdout).unwrap();
    assert_eq!(checked["checks"][0]["code"], "backup_copy_missing");
    assert_eq!(checked["checks"][0]["recorded"], true);
    let report = assert_doctor_block_is_status(root.path());
    assert_eq!(
        report["backup"]["kinds"][0]["reason"],
        "backup_copy_missing"
    );
    // A missing backup leaves a healthy store healthy.
    assert_eq!(report["healthy"], true);
}

#[test]
fn a_stale_or_missing_backup_leaves_a_healthy_store_healthy() {
    let root = test_support::temp_home().unwrap();
    setup(root.path());
    // A target with no copy yet: the mode is local.
    set_target(root.path(), &root.path().join("copies"));
    let (report, plain) = doctor(root.path());
    assert_eq!(report["healthy"], true);
    assert_eq!(report["backup"]["durability"]["mode"], "local");
    assert_eq!(
        report["backup"]["kinds"][0]["reason"],
        "backup_never_confirmed"
    );
    assert!(plain.contains("Engram store is healthy"), "{plain}");
    assert!(
        plain.contains("store: does not qualify: backup_never_confirmed"),
        "{plain}"
    );
}

#[test]
fn a_refused_store_still_carries_the_backup_block() {
    let root = test_support::temp_home().unwrap();
    let database = setup(root.path());
    set_target(root.path(), &root.path().join("copies"));
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch("DROP INDEX memory_heads_scope")
        .unwrap();
    drop(connection);

    let json = engram(root.path(), &["doctor", "--json"]);
    assert_eq!(json.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(value["code"], "projection_repair_required");
    assert_eq!(value["healthy"], false);
    assert_eq!(value["backup"]["durability"]["mode"], "local");
    assert_eq!(
        value["backup"]["kinds"][0]["reason"],
        "backup_never_confirmed"
    );
    let plain = engram(root.path(), &["doctor"]);
    assert_eq!(plain.status.code(), Some(1));
    let printed = text(&plain.stdout);
    assert!(
        printed.contains("code: \"projection_repair_required\""),
        "{printed}"
    );
    assert!(printed.contains("\nbackup mode: local ("), "{printed}");
}

/// Readiness, with the backup records present and then removed, must read
/// the same, healthy or refused.
fn readiness_with_and_without_target(root: &Path, ready: bool) {
    let with = engram(root, &["readiness", "--json"]);
    assert_eq!(with.status.success(), ready, "{}", text(&with.stderr));
    let records = root.join("home").join("backup-records");
    assert!(records.exists());
    fs::remove_dir_all(&records).unwrap();
    let without = engram(root, &["readiness", "--json"]);
    assert_eq!(with.stdout, without.stdout);
    assert_eq!(with.status.code(), without.status.code());
    let value: Value = serde_json::from_slice(&with.stdout).unwrap();
    assert!(!has_backup_field(&value), "{value}");
}

/// Whether any field name in `value` speaks of backups.
fn has_backup_field(value: &Value) -> bool {
    match value {
        Value::Object(fields) => fields
            .iter()
            .any(|(name, field)| name.contains("backup") || has_backup_field(field)),
        Value::Array(items) => items.iter().any(has_backup_field),
        _ => false,
    }
}

#[test]
fn readiness_reads_the_same_with_and_without_a_configured_target() {
    let root = test_support::temp_home().unwrap();
    setup(root.path());
    set_target(root.path(), &root.path().join("copies"));
    succeeded(root.path(), &["backup", "push"]);
    readiness_with_and_without_target(root.path(), true);

    // A refused store reads the same too.
    let root = test_support::temp_home().unwrap();
    let database = setup(root.path());
    set_target(root.path(), &root.path().join("copies"));
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch("DROP INDEX memory_heads_scope")
        .unwrap();
    drop(connection);
    readiness_with_and_without_target(root.path(), false);
}
