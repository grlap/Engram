//! Observable backup status JSON across unavailable, failed and restore states.

#[path = "../src/test_support.rs"]
mod test_support;

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use engram::{
    ObjectId, ProjectId,
    backup::{
        CopyKind,
        restore::{RestoreRecord, RestoreState, restore_record_path, write_restore_record},
        target::{PushLock, RECORD_FORMAT_VERSION, RecordPaths, Statement},
    },
    project_digest,
};
use serde_json::Value;

const PROJECT: &str = "status-receipt-fixture";

/// One home under the test's root, with the project file beside it.
struct Home {
    root: PathBuf,
    name: &'static str,
}

impl Home {
    fn new(root: &Path, name: &'static str) -> Self {
        fs::write(root.join(".engram-project"), format!("{PROJECT}\n")).unwrap();
        Self {
            root: root.to_path_buf(),
            name,
        }
    }

    fn home(&self) -> PathBuf {
        self.root.join(self.name)
    }

    fn copies(&self) -> PathBuf {
        self.root.join(format!("{}-copies", self.name))
    }

    fn engram(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_engram"))
            .current_dir(&self.root)
            .env_remove("ENGRAM_HOME")
            .arg("--home")
            .arg(self.home())
            .arg("--project-file")
            .arg(self.root.join(".engram-project"))
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

    fn status(&self, check_target: bool) -> Value {
        let mut args = vec!["backup", "status", "--json"];
        if check_target {
            args.push("--check-target");
        }
        let value: Value = serde_json::from_slice(&self.succeeded(&args).stdout).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value.get("checks").is_some(), check_target, "{value}");
        value
    }

    /// An initialized store, a directory target and one pushed copy.
    fn pushed(&self) {
        self.succeeded(&["init"]);
        fs::create_dir_all(self.copies()).unwrap();
        self.succeeded(&[
            "backup",
            "target",
            "set",
            "--kind",
            "store",
            "--adapter",
            "directory",
            "--dir",
            self.copies().to_str().unwrap(),
            "--disclosure-authorized-by",
            "greg",
            "--off-host-asserted-by",
            "greg",
        ]);
        self.succeeded(&["backup", "push"]);
    }

    fn paths(&self) -> RecordPaths {
        RecordPaths::new(&self.home(), &ProjectId(PROJECT.into()), CopyKind::Store)
    }

    fn state(&self) -> Value {
        serde_json::from_slice(&fs::read(self.paths().state).unwrap()).unwrap()
    }

    fn write_state(&self, state: &Value) {
        fs::write(
            self.paths().state,
            serde_json::to_vec_pretty(state).unwrap(),
        )
        .unwrap();
    }

    fn write_restore(&self, state: RestoreState) {
        let project = ProjectId(PROJECT.into());
        let lock = PushLock::try_acquire(&self.paths()).unwrap();
        let now = chrono::Utc::now();
        write_restore_record(
            &self.home(),
            &project,
            &lock,
            &RestoreRecord {
                format_version: RECORD_FORMAT_VERSION,
                occurrence_id: uuid::Uuid::now_v7(),
                project: PROJECT.into(),
                source: engram::backup::restore::RestoreSource::Copy {
                    copy: "20261002T000000Z-receipt".into(),
                    origin_host: Some("old-host".into()),
                    origin_retired: Statement {
                        by: "greg".into(),
                        at: now,
                    },
                },
                sha256: "ab".repeat(32),
                staging: "staging".into(),
                state,
                pending_at: now,
                completed_at: (state == RestoreState::Completed).then_some(now),
                previous_completed: (state == RestoreState::Pending).then(|| {
                    engram::backup::restore::PreviousRestore {
                        occurrence_id: uuid::Uuid::now_v7(),
                        file: "store.restore-20261002T000000Z.json".into(),
                    }
                }),
            },
        )
        .unwrap();
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Exercise status transitions through the real CLI.
fn status_cases(root: &Path) {
    // No store at all: the store's cut is unavailable, no target.
    let bare = Home::new(root, "bare");
    let missing = bare.status(false);
    assert_eq!(
        missing["store_cut_unavailable"]["code"], "store_not_initialized",
        "{missing}"
    );

    // A pushed copy; then a failed push leaves the copy qualifying, and the
    // failed last attempt is shown beside `local_backed_up`.
    let failing = Home::new(root, "failing");
    failing.pushed();
    let pushed = failing.status(false);
    // An absent value is present as null, never left out.
    let target = &pushed["kinds"][0]["target"];
    for key in ["pending", "copy", "last_attempt"] {
        assert!(target.get(key).is_some(), "{key}: {target}");
    }
    assert_eq!(target["pending"], Value::Null);
    assert_eq!(pushed["restore"], Value::Null);
    let aside = root.join("failing-copies-gone");
    fs::rename(failing.copies(), &aside).unwrap();
    let push = failing.engram(&["backup", "push"]);
    assert!(!push.status.success(), "the push must fail");
    let failed = failing.status(false);
    assert_eq!(failed["durability"]["mode"], "local_backed_up", "{failed}");
    let attempt = &failed["kinds"][0]["target"]["last_attempt"];
    assert_eq!(attempt["outcome"], "failed", "{attempt}");
    assert!(
        attempt["code"].is_string() && attempt["message"].is_string(),
        "{attempt}"
    );

    // A pending attempt and a copy checked by another build, then a copy
    // the target no longer holds, found by a check.
    let edited = Home::new(root, "edited");
    edited.pushed();
    let mut state = edited.state();
    let other_build = ObjectId::from_canonical_bytes(b"another build")
        .as_str()
        .to_owned();
    state["newest_receipt"]["manifest"]["capture"]["build_fingerprint"] =
        other_build.clone().into();
    for receipt in state["receipts"].as_array_mut().unwrap() {
        receipt["manifest"]["capture"]["build_fingerprint"] = other_build.clone().into();
    }
    let manifest = state["newest_receipt"]["manifest"].clone();
    state["pending"] = serde_json::json!({
        "id": "01a0fe00-0000-7000-8000-000000000000",
        "manifest": manifest,
        "data_file": "pending.db.gz",
        "temporary_data_file": "pending.db.gz.tmp",
    });
    edited.write_state(&state);
    let checked_by_another = edited.status(false);
    let copy = &checked_by_another["kinds"][0]["target"]["copy"];
    assert_eq!(copy["checking_build"], other_build.as_str(), "{copy}");
    assert!(checked_by_another["kinds"][0]["target"]["pending"].is_object());
    let data = edited
        .copies()
        .join(project_digest(&ProjectId(PROJECT.into())))
        .join(format!("{}.db.gz", manifest["copy"].as_str().unwrap()));
    fs::remove_file(data).unwrap();
    let checked = edited.status(true);
    assert_eq!(checked["checks"][0]["outcome"], "missing", "{checked}");
    assert!(checked["kinds"][0]["target"]["copy"]["missing"].is_object());

    // A recorded restore: pending, restored, and a record that cannot be
    // used. The outer `restored` is the record's `completed`.
    edited.write_restore(RestoreState::Pending);
    let pending = edited.status(false);
    assert_eq!(pending["restore"]["state"], "pending");
    assert_eq!(pending["restore"]["record"]["state"], "pending");
    edited.write_restore(RestoreState::Completed);
    let restored = edited.status(false);
    assert_eq!(restored["restore"]["state"], "restored");
    assert_eq!(restored["restore"]["record"]["state"], "completed");

    fs::write(
        restore_record_path(&edited.home(), &ProjectId(PROJECT.into())),
        b"not a record",
    )
    .unwrap();
    let unreadable = edited.status(false);
    assert_eq!(unreadable["restore"]["state"], "unreadable");
    assert_eq!(unreadable["restore"]["record"], Value::Null);
    assert!(unreadable["restore"]["unreadable"].is_string());

    // A state file this build cannot use.
    let broken = Home::new(root, "broken");
    broken.pushed();
    fs::write(broken.paths().state, b"{ not json").unwrap();
    let broken_status = broken.status(false);
    assert!(broken_status["kinds"][0]["unreadable"].is_string());
}

#[test]
fn status_reports_unavailable_backup_and_restore_states() {
    let root = test_support::temp_home().unwrap();
    status_cases(root.path());
}
