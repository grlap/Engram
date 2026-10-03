//! `backup push` captures the store every time, and when the settled copy's
//! bytes equal the newest copy, which this build checked in full, it skips
//! the full check and the upload. Every kind of change an agent makes to the
//! store gives other bytes, so the next push checks and uploads in full.

#[path = "../src/test_support.rs"]
mod test_support;

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use serde_json::Value;

const PROJECT: &str = "change-signal-fixture";

struct Home {
    root: test_support::TempHome,
}

impl Home {
    /// An initialized store with a directory target.
    fn configured() -> Self {
        let root = test_support::temp_home().unwrap();
        fs::write(root.path().join(".engram-project"), format!("{PROJECT}\n")).unwrap();
        let copies = root.path().join("copies");
        fs::create_dir_all(&copies).unwrap();
        let home = Self { root };
        home.succeeded(&["init"]);
        home.succeeded(&[
            "backup",
            "target",
            "set",
            "--kind",
            "store",
            "--adapter",
            "directory",
            "--dir",
            copies.to_str().unwrap(),
            "--disclosure-authorized-by",
            "greg",
            "--off-host-asserted-by",
            "greg",
        ]);
        home
    }

    fn engram(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_engram"))
            .current_dir(self.root.path())
            .env_remove("ENGRAM_HOME")
            .env_remove("ENGRAM_SESSION_ID")
            .env_remove("ENGRAM_ACTOR_ID")
            .env_remove("ENGRAM_ACTOR_CONTEXT")
            .arg("--home")
            .arg(self.root.path().join("home"))
            .arg("--project-file")
            .arg(self.root.path().join(".engram-project"))
            .args(args)
            .output()
            .expect("run engram")
    }

    fn succeeded(&self, args: &[&str]) -> String {
        let output = self.engram(args);
        assert!(
            output.status.success(),
            "{args:?}: {}{}",
            text(&output.stdout),
            text(&output.stderr)
        );
        text(&output.stdout)
    }

    /// A work word as the fixture agent.
    fn work(&self, args: &[&str]) -> String {
        let mut all = vec![
            "work",
            "--actor-id",
            "change-agent",
            "--session-id",
            "change-session",
        ];
        all.extend_from_slice(args);
        self.succeeded(&all)
    }

    /// Pushes and returns the one kind's report, which must have succeeded.
    fn push(&self) -> Value {
        let report: Value =
            serde_json::from_str(&self.succeeded(&["backup", "push", "--json"])).unwrap();
        report["kinds"][0].clone()
    }

    /// Pushes after a change: the copy is checked in full and uploaded.
    fn push_changed(&self, change: &str) {
        let report = self.push();
        assert_eq!(report["outcome"], "uploaded", "{change}: {report}");
        assert_eq!(report["capture_check"], "full", "{change}: {report}");
    }

    /// Pushes with nothing changed: the newest copy's check stands, and the
    /// target confirms the copy instead of receiving a new one.
    fn push_unchanged(&self, after: &str) {
        let report = self.push();
        assert_eq!(report["outcome"], "unchanged", "after {after}: {report}");
        assert_eq!(
            report["capture_check"], "same_bytes_as_newest",
            "after {after}: {report}"
        );
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The first work reference a receipt names.
fn work_ref(receipt: &str) -> String {
    let start = receipt.find("w-").expect("a work reference");
    receipt[start..start + 14].to_owned()
}

#[test]
fn every_kind_of_agent_change_makes_the_next_push_check_and_upload_in_full() {
    let home = Home::configured();
    home.push_changed("the first push");
    home.push_unchanged("the first push");

    let added = home.work(&["add", "A change the backup must see"]);
    let item = work_ref(&added);
    home.push_changed("a new work item");
    home.push_unchanged("a new work item");

    home.work(&["claim", &item]);
    home.push_changed("a claim");
    home.push_unchanged("a claim");

    home.work(&["claim", &item, "--ttl", "7200"]);
    home.push_changed("a claim renewal");
    home.push_unchanged("a claim renewal");

    home.work(&["next"]);
    home.push_changed("a delivery");
    home.push_unchanged("a delivery");

    home.work(&["note", "a note on the held item"]);
    home.push_changed("a note");
    home.push_unchanged("a note");

    home.work(&["remember", "a project memory", "--key", "change-memory"]);
    home.push_changed("a project memory");
    home.push_unchanged("a project memory");
}

#[test]
fn a_push_without_a_target_reports_no_capture_check() {
    let root = test_support::temp_home().unwrap();
    fs::write(root.path().join(".engram-project"), format!("{PROJECT}\n")).unwrap();
    let home = Home { root };
    home.succeeded(&["init"]);
    let report = home.push();
    assert_eq!(report["outcome"], "not_configured");
    assert_eq!(report["capture_check"], Value::Null);
    assert!(!Path::new(&home.root.path().join("home").join("backup-stage")).exists());
}
