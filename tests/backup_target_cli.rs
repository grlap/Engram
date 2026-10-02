#[path = "../src/test_support.rs"]
mod test_support;

use std::{
    path::Path,
    process::{Command, Output},
};

use serde_json::Value;

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_engram"))
        .arg("--home")
        .arg(home)
        .args(args)
        .output()
        .expect("run engram")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn absolute_dir() -> &'static str {
    if cfg!(windows) {
        r"D:\engram-copies"
    } else {
        "/srv/engram-copies"
    }
}

fn set_args<'a>(dir: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec![
        "backup",
        "target",
        "set",
        "--kind",
        "store",
        "--adapter",
        "directory",
        "--dir",
        dir,
    ];
    args.extend_from_slice(extra);
    args
}

#[test]
fn backup_target_words_set_show_and_clear_a_target() {
    let home = test_support::temp_home().unwrap();

    // A clean home has no target, and showing it creates nothing.
    let shown = run(home.path(), &["backup", "target", "show"]);
    assert!(shown.status.success(), "{}", text(&shown.stderr));
    assert_eq!(
        text(&shown.stdout).trim(),
        "No backup target configured for this project."
    );
    let shown = run(home.path(), &["backup", "target", "show", "--json"]);
    let value: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(value["targets"], Value::Array(Vec::new()));
    assert!(!home.path().join("backup-records").exists());
    assert!(!home.path().join("projects").exists());

    let set = run(
        home.path(),
        &set_args(
            absolute_dir(),
            &[
                "--disclosure-authorized-by",
                "greg",
                "--off-host-asserted-by",
                "greg",
                "--window-hours",
                "12",
                "--keep",
                "5",
            ],
        ),
    );
    assert!(set.status.success(), "{}", text(&set.stderr));
    assert!(text(&set.stdout).contains("off-host asserted; not verified"));

    let shown = run(home.path(), &["backup", "target", "show"]);
    let shown = text(&shown.stdout);
    assert!(shown.contains(absolute_dir()), "{shown}");
    assert!(
        shown.contains("disclosure authorized by greg at "),
        "{shown}"
    );
    assert!(
        shown.contains("off-host asserted; not verified: stated by greg at "),
        "{shown}"
    );
    assert!(shown.contains("window: 12 h; keep: 5 copies"), "{shown}");
    let shown = run(home.path(), &["backup", "target", "show", "--json"]);
    let value: Value = serde_json::from_slice(&shown.stdout).unwrap();
    let target = &value["targets"][0];
    assert_eq!(target["kind"], "store");
    assert_eq!(target["adapter"], "directory");
    assert_eq!(target["dir"], absolute_dir());
    assert_eq!(target["disclosure_authorized"]["by"], "greg");
    assert_eq!(target["off_host_asserted"]["by"], "greg");
    assert!(target["disclosure_authorized"]["at"].is_string());
    assert_eq!(target["off_host"], "off-host asserted; not verified");
    assert!(target["identity"].is_string());
    // The words never open or create the store.
    assert!(!home.path().join("projects").exists());

    let cleared = run(
        home.path(),
        &["backup", "target", "clear", "--kind", "store"],
    );
    assert!(cleared.status.success(), "{}", text(&cleared.stderr));
    assert!(text(&cleared.stdout).contains("backup target cleared: store"));
    let shown = run(home.path(), &["backup", "target", "show"]);
    assert_eq!(
        text(&shown.stdout).trim(),
        "No backup target configured for this project."
    );
}

#[test]
fn backup_target_set_refuses_missing_statements_and_a_relative_path() {
    let home = test_support::temp_home().unwrap();
    let refused = |args: Vec<&str>, expected: &str| {
        let output = run(home.path(), &args);
        assert!(!output.status.success(), "{args:?}");
        let stderr = text(&output.stderr);
        assert!(stderr.contains(expected), "{args:?}: {stderr}");
    };
    refused(
        set_args(absolute_dir(), &["--off-host-asserted-by", "greg"]),
        "--disclosure-authorized-by",
    );
    refused(
        set_args(absolute_dir(), &["--disclosure-authorized-by", "greg"]),
        "backup_target_invalid: a directory target needs --off-host-asserted-by",
    );
    refused(
        set_args(
            "relative-copies",
            &[
                "--disclosure-authorized-by",
                "greg",
                "--off-host-asserted-by",
                "greg",
            ],
        ),
        "backup_target_invalid: --dir must be an absolute path",
    );
    assert!(!home.path().join("backup-records").exists());
}

#[test]
fn plain_backup_keeps_writing_a_copy_and_refuses_out_with_a_target_word() {
    let home = test_support::temp_home().unwrap();
    let init = run(home.path(), &["init"]);
    assert!(init.status.success(), "{}", text(&init.stderr));
    let copy = home.path().join("copy.db");
    let backup = run(home.path(), &["backup", "--out", copy.to_str().unwrap()]);
    assert!(backup.status.success(), "{}", text(&backup.stderr));
    assert!(copy.is_file());

    let mixed = run(
        home.path(),
        &["backup", "--out", copy.to_str().unwrap(), "target", "show"],
    );
    assert!(!mixed.status.success());
    assert!(text(&mixed.stderr).contains("cannot be used with"));
}
