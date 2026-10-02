#[path = "../src/test_support.rs"]
mod test_support;

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use engram::{
    ProjectId,
    backup::{CopyKind, target::RecordPaths},
};
use serde_json::Value;

const PROJECT: &str = "status-cli-fixture";

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

/// Runs `backup status` in text and JSON; both must succeed.
fn status(root: &Path) -> (String, Value) {
    let plain = engram(root, &["backup", "status"]);
    assert!(plain.status.success(), "{}", text(&plain.stderr));
    let json = engram(root, &["backup", "status", "--json"]);
    assert!(json.status.success(), "{}", text(&json.stderr));
    let value: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    // The mode is only ever reported beside its off-host field.
    assert!(value.get("mode").is_none(), "{value}");
    assert!(value["durability"]["off_host"].is_array(), "{value}");
    (text(&plain.stdout), value)
}

#[test]
fn status_without_a_target_reads_local_and_creates_nothing() {
    let root = test_support::temp_home().unwrap();
    setup(root.path());
    let (plain, value) = status(root.path());
    assert_eq!(
        plain.lines().next(),
        Some("backup mode: local (no copy qualifies; nothing is known to be held off this host)")
    );
    assert_eq!(value["durability"]["mode"], "local");
    assert_eq!(value["kinds"][0]["reason"], "backup_not_configured");
    assert!(!root.path().join("home").join("backup-records").exists());
}

#[test]
fn status_after_a_push_reads_local_backed_up_with_the_directory_off_host_text() {
    let root = test_support::temp_home().unwrap();
    setup(root.path());
    let copies = root.path().join("copies");
    fs::create_dir_all(&copies).unwrap();
    set_target(root.path(), &copies);
    let (plain, value) = status(root.path());
    assert_eq!(value["kinds"][0]["reason"], "backup_never_confirmed");
    assert!(plain.contains("no copy confirmed yet"), "{plain}");

    let push = engram(root.path(), &["backup", "push"]);
    assert!(push.status.success(), "{}", text(&push.stderr));
    let (plain, value) = status(root.path());
    assert_eq!(value["durability"]["mode"], "local_backed_up");
    assert_eq!(
        value["durability"]["off_host"][0]["off_host"],
        "off-host asserted; not verified"
    );
    assert_eq!(value["kinds"][0]["qualifies"], true);
    assert_eq!(
        value["kinds"][0]["target"]["off_host"],
        "off-host asserted; not verified"
    );
    let first = plain.lines().next().unwrap();
    assert!(
        first.starts_with("backup mode: local_backed_up ("),
        "{first}"
    );
    assert!(first.contains("off-host asserted; not verified"), "{first}");
    for expected in [
        "store: qualifies",
        "copy: ",
        "capture started ",
        "last confirmed at ",
    ] {
        assert!(plain.contains(expected), "{expected}\n{plain}");
    }

    // Status reads only local evidence: with the target gone it still
    // reports what was recorded, and creates nothing there.
    fs::remove_dir_all(&copies).unwrap();
    let (_, value) = status(root.path());
    assert_eq!(value["durability"]["mode"], "local_backed_up");
    assert!(!copies.exists());
}

#[test]
fn an_unreadable_state_file_is_a_typed_reason_and_not_a_crash() {
    let root = test_support::temp_home().unwrap();
    setup(root.path());
    let copies = root.path().join("copies");
    fs::create_dir_all(&copies).unwrap();
    set_target(root.path(), &copies);
    let paths = RecordPaths::new(
        &root.path().join("home"),
        &ProjectId(PROJECT.into()),
        CopyKind::Store,
    );
    fs::write(&paths.state, b"not json").unwrap();
    let (plain, value) = status(root.path());
    assert_eq!(value["durability"]["mode"], "local");
    assert_eq!(value["kinds"][0]["reason"], "backup_record_unreadable");
    assert!(
        value["kinds"][0]["unreadable"]
            .as_str()
            .unwrap()
            .contains("store.state.json"),
        "{value}"
    );
    assert!(
        plain.contains("store: does not qualify: backup_record_unreadable"),
        "{plain}"
    );
    assert_eq!(fs::read(&paths.state).unwrap(), b"not json");
}

#[test]
fn a_state_with_an_impossible_cut_is_unreadable_and_status_does_not_crash() {
    let root = test_support::temp_home().unwrap();
    setup(root.path());
    let copies = root.path().join("copies");
    fs::create_dir_all(&copies).unwrap();
    set_target(root.path(), &copies);
    let push = engram(root.path(), &["backup", "push"]);
    assert!(push.status.success(), "{}", text(&push.stderr));
    let paths = RecordPaths::new(
        &root.path().join("home"),
        &ProjectId(PROJECT.into()),
        CopyKind::Store,
    );
    for field in ["work_feed", "project_memory"] {
        let mut state: Value = serde_json::from_slice(&fs::read(&paths.state).unwrap()).unwrap();
        let original = state.clone();
        state["newest_receipt"]["manifest"]["capture"]["cut"][field] = Value::from(i64::MIN);
        fs::write(&paths.state, serde_json::to_vec(&state).unwrap()).unwrap();
        let (plain, value) = status(root.path());
        assert_eq!(
            value["kinds"][0]["reason"], "backup_record_unreadable",
            "{field}"
        );
        assert!(plain.contains("negative position"), "{field}: {plain}");
        fs::write(&paths.state, serde_json::to_vec(&original).unwrap()).unwrap();
    }
}
