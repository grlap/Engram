//! Each kind of backup trouble the operator must see: a failed push, a
//! refused capture, a copy the target no longer holds, a stale copy and an
//! expired confirmation. Each
//! is checked in `backup status --json`, in `doctor --json`'s backup block,
//! and as the one backup reminder line of `next --peek`.

#[path = "../src/test_support.rs"]
mod test_support;

use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};

use engram::{
    ProjectId,
    backup::{CopyKind, target::RecordPaths},
    project_database_path, project_digest,
};
use serde_json::Value;

const PROJECT: &str = "visibility-fixture";
const DIRECTORY: &str = "off-host asserted; not verified";

struct Home {
    root: test_support::TempHome,
}

impl Home {
    /// An initialized store with a directory target.
    fn configured() -> Self {
        let root = test_support::temp_home().unwrap();
        fs::write(root.path().join(".engram-project"), format!("{PROJECT}\n")).unwrap();
        let home = Self { root };
        fs::create_dir_all(home.copies()).unwrap();
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
            home.copies().to_str().unwrap(),
            "--disclosure-authorized-by",
            "greg",
            "--off-host-asserted-by",
            "greg",
            // The stale case is written against this window.
            "--window-hours",
            "24",
        ]);
        home
    }

    /// A configured store with one pushed copy.
    fn pushed() -> Self {
        let home = Self::configured();
        home.succeeded(&["backup", "push"]);
        home
    }

    fn copies(&self) -> PathBuf {
        self.root.path().join("copies")
    }

    fn engram(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_engram"))
            .current_dir(self.root.path())
            .env_remove("ENGRAM_HOME")
            .env_remove("ENGRAM_SESSION_ID")
            .env_remove("ENGRAM_ACTOR_ID")
            .arg("--home")
            .arg(self.root.path().join("home"))
            .arg("--project-file")
            .arg(self.root.path().join(".engram-project"))
            .args(args)
            .output()
            .expect("run engram")
    }

    fn succeeded(&self, args: &[&str]) -> Output {
        let output = self.engram(args);
        assert!(
            output.status.success(),
            "{args:?}: {}{}",
            text(&output.stdout),
            text(&output.stderr)
        );
        output
    }

    fn status(&self) -> Value {
        serde_json::from_slice(&self.succeeded(&["backup", "status", "--json"]).stdout).unwrap()
    }

    /// The backup block of `doctor --json` on the healthy store, which must
    /// equal the status receipt apart from the time it was evaluated.
    fn doctor_block(&self) -> Value {
        let output = self.succeeded(&["doctor", "--json"]);
        let report: Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|error| panic!("{error}: {}", text(&output.stdout)));
        assert_eq!(report["healthy"], true, "{report}");
        report["backup"].clone()
    }

    fn database(&self) -> PathBuf {
        project_database_path(&self.root.path().join("home"), &ProjectId(PROJECT.into()))
    }

    /// The backup lines `next --peek` prints.
    fn reminder_lines(&self) -> Vec<String> {
        let output = self.succeeded(&[
            "work",
            "--actor-id",
            "visibility-agent",
            "--session-id",
            "visibility-session",
            "next",
            "--peek",
        ]);
        text(&output.stdout)
            .lines()
            .map(|line| line.trim().trim_start_matches("- ").to_owned())
            .filter(|line| line.starts_with("backup:"))
            .collect()
    }

    fn state_path(&self) -> PathBuf {
        RecordPaths::new(
            &self.root.path().join("home"),
            &ProjectId(PROJECT.into()),
            CopyKind::Store,
        )
        .state
    }

    /// Checks the three surfaces: the doctor block equals the status receipt
    /// apart from the time each was evaluated, and `next` prints exactly
    /// `line`.
    fn assert_visible(&self, line: &str) -> Value {
        let status = self.status();
        let doctor = self.doctor_block();
        assert_eq!(timeless(&doctor), timeless(&status));
        assert_eq!(self.reminder_lines(), [line.to_owned()]);
        status
    }
}

/// A receipt without the fields that depend on when it was evaluated.
fn timeless(receipt: &Value) -> Value {
    let mut receipt = receipt.clone();
    receipt.as_object_mut().unwrap().remove("as_of");
    for kind in receipt["kinds"].as_array_mut().unwrap() {
        if let Some(copy) = kind["target"]["copy"].as_object_mut() {
            copy.remove("capture_age_seconds");
        }
    }
    receipt
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn a_failed_push_is_seen_beside_the_copy_that_still_qualifies() {
    let home = Home::pushed();
    let aside = home.root.path().join("copies-gone");
    fs::rename(home.copies(), &aside).unwrap();
    let push = home.engram(&["backup", "push"]);
    assert!(!push.status.success(), "the push must fail");
    // The failure is typed as the target's either way: when the second
    // capture equals the newest copy the push asks the target to confirm it
    // (backup_target_unconfirmed); when it differs the push puts a new copy,
    // whose missing root is unreachable (backup_target_unreachable).
    let recorded = home.status();
    let code = recorded["kinds"][0]["target"]["last_attempt"]["code"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        ["backup_target_unconfirmed", "backup_target_unreachable"].contains(&code.as_str()),
        "{code}"
    );

    let status = home.assert_visible(&format!(
        "backup: the last store push failed: {code}; an earlier copy still qualifies (store copy: {DIRECTORY}); see engram backup status"
    ));
    assert_eq!(status["durability"]["mode"], "local_backed_up");
    assert_eq!(status["durability"]["off_host"][0]["off_host"], DIRECTORY);
    let attempt = &status["kinds"][0]["target"]["last_attempt"];
    assert_eq!(attempt["outcome"], "failed");
}

#[test]
fn a_capture_the_store_refuses_is_seen_as_a_failed_push_with_the_store_s_code() {
    let home = Home::configured();
    // The store refuses the capture: its file is not there while the push
    // runs. It is put back before the surfaces are read.
    let database = home.database();
    let aside = home.root.path().join("engram.db.aside");
    fs::rename(&database, &aside).unwrap();
    let push = home.engram(&["backup", "push"]);
    fs::rename(&aside, &database).unwrap();
    assert!(!push.status.success(), "the capture must be refused");

    let status = home.assert_visible(
        "backup: mode local (store: backup_never_confirmed); the last store push failed: store_not_initialized; see engram backup status",
    );
    assert_eq!(status["durability"]["mode"], "local");
    let attempt = &status["kinds"][0]["target"]["last_attempt"];
    assert_eq!(attempt["outcome"], "failed");
    assert_eq!(attempt["code"], "store_not_initialized");
}

#[test]
fn a_capture_that_passes_its_deadline_is_seen_as_a_failed_push_with_its_code() {
    let home = Home::configured();
    let push = home.engram(&["backup", "push", "--capture-deadline-secs", "0"]);
    assert!(!push.status.success(), "the capture must be refused");

    let status = home.assert_visible(
        "backup: mode local (store: backup_never_confirmed); the last store push failed: backup_capture_deadline; see engram backup status",
    );
    assert_eq!(status["durability"]["mode"], "local");
    let attempt = &status["kinds"][0]["target"]["last_attempt"];
    assert_eq!(attempt["outcome"], "failed");
    assert_eq!(attempt["code"], "backup_capture_deadline");
}

#[test]
fn a_copy_the_target_no_longer_holds_is_seen_once_a_check_records_it() {
    let home = Home::pushed();
    let state: Value = serde_json::from_slice(&fs::read(home.state_path()).unwrap()).unwrap();
    let copy = state["newest_receipt"]["manifest"]["copy"]
        .as_str()
        .unwrap();
    fs::remove_file(
        home.copies()
            .join(project_digest(&ProjectId(PROJECT.into())))
            .join(format!("{copy}.db.gz")),
    )
    .unwrap();
    home.succeeded(&["backup", "status", "--check-target", "--json"]);

    let status = home.assert_visible(
        "backup: mode local (store: backup_copy_missing); see engram backup status",
    );
    assert_eq!(status["kinds"][0]["reason"], "backup_copy_missing");
    assert!(status["kinds"][0]["target"]["copy"]["missing"].is_object());
}

#[test]
fn a_copy_older_than_the_window_is_seen_as_stale() {
    let home = Home::pushed();
    // The copy's content was last seen two days ago, beyond the 24-hour
    // window; its target confirmation stays recent.
    let mut state: Value = serde_json::from_slice(&fs::read(home.state_path()).unwrap()).unwrap();
    let old = (chrono::Utc::now() - chrono::Duration::hours(48)).to_rfc3339();
    state["newest_receipt"]["manifest"]["capture"]["capture_started_at"] = old.clone().into();
    for receipt in state["receipts"].as_array_mut().unwrap() {
        receipt["manifest"]["capture"]["capture_started_at"] = old.clone().into();
    }
    state["observed_equal_at"] = old.into();
    fs::write(
        home.state_path(),
        serde_json::to_vec_pretty(&state).unwrap(),
    )
    .unwrap();

    let status =
        home.assert_visible("backup: mode local (store: backup_stale); see engram backup status");
    assert_eq!(status["kinds"][0]["reason"], "backup_stale");
    assert_eq!(status["durability"]["mode"], "local");
}

#[test]
fn a_confirmation_older_than_the_window_is_seen_as_expired() {
    let home = Home::pushed();
    // The target last confirmed the copy two days ago, beyond the 24-hour
    // window; the store's content was observed in it just now.
    let mut state: Value = serde_json::from_slice(&fs::read(home.state_path()).unwrap()).unwrap();
    let old = (chrono::Utc::now() - chrono::Duration::hours(48)).to_rfc3339();
    assert!(state["last_confirmation"]["at"].is_string(), "{state}");
    state["last_confirmation"]["at"] = old.into();
    fs::write(
        home.state_path(),
        serde_json::to_vec_pretty(&state).unwrap(),
    )
    .unwrap();

    // Doctor stays healthy: an expired confirmation is a backup finding, not
    // a store fault.
    let status = home.assert_visible(
        "backup: mode local (store: backup_confirmation_expired); see engram backup status",
    );
    assert_eq!(status["kinds"][0]["reason"], "backup_confirmation_expired");
    assert_eq!(status["durability"]["mode"], "local");
}
