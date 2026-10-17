//! `task-sync export|import --json` refusals carry stable codes while the
//! plain form keeps its human text.

#[path = "../src/test_support.rs"]
mod test_support;

#[path = "common/task_sync_opt_in.rs"]
mod task_sync_opt_in;

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::Value;

const PROJECT: &str = "task-sync-refusal-codes";

#[path = "task_sync_refusal_codes/boundaries.rs"]
mod boundaries;

struct Fixture {
    _home: test_support::TempHome,
    root: PathBuf,
    marker: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let home = test_support::temp_home().unwrap();
        let root = home.path().to_path_buf();
        let marker = root.join(".engram-project");
        fs::write(&marker, format!("{PROJECT}\n")).unwrap();
        Self {
            _home: home,
            root,
            marker,
        }
    }

    /// An initialized, opted-in store home.
    fn store(&self, name: &str) -> PathBuf {
        let home = self.root.join(name);
        task_sync_opt_in::enable(&home, PROJECT);
        let output = self.engram(&home).arg("init").output().unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        home
    }

    fn engram(&self, home: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_engram"));
        command
            .arg("--home")
            .arg(home)
            .arg("--project-file")
            .arg(&self.marker)
            .env_remove("ENGRAM_ACTOR_CONTEXT");
        command
    }

    fn sync(&self, home: &Path, operation: &str, path: &Path, json: bool) -> Output {
        self.sync_command(home, operation, path, json)
            .output()
            .unwrap()
    }

    fn sync_command(&self, home: &Path, operation: &str, path: &Path, json: bool) -> Command {
        let mut command = self.engram(home);
        command
            .args([
                "task-sync",
                "--actor-id",
                "operator",
                "--session-id",
                "sync-operator",
                operation,
            ])
            .arg(path);
        if json {
            command.arg("--json");
        }
        command
    }

    fn add(&self, home: &Path, title: &str) {
        let output = self
            .engram(home)
            .args([
                "work",
                "--actor-id",
                "author",
                "--session-id",
                "author-session",
                "add",
                title,
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
    }

    /// A candidate exported from a fresh store holding one task.
    fn candidate(&self, name: &str) -> PathBuf {
        let source = self.store(&format!("{name}-source"));
        self.add(&source, "shared task");
        let path = self.root.join(name);
        let output = self.sync(&source, "export", &path, false);
        assert!(output.status.success(), "{}", stderr(&output));
        path
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The structured refusal printed on stderr, after any plain notices.
fn refusal(output: &Output) -> Value {
    assert!(!output.status.success(), "the command was refused");
    let text = stderr(output);
    let start = text
        .find("{\n")
        .unwrap_or_else(|| panic!("no JSON refusal on stderr: {text}"));
    serde_json::from_str(&text[start..]).unwrap()
}

/// The same refusal without `--json` prints the same human text.
fn assert_plain_text_kept(output: &Output, refused: &Value) {
    assert!(!output.status.success());
    let message = refused["error"]["message"].as_str().unwrap();
    assert!(
        stderr(output).contains(message),
        "plain stderr {:?} lacks {message:?}",
        stderr(output)
    );
}

#[test]
fn task_sync_import_refusals_carry_stable_codes_with_unchanged_text() {
    let fixture = Fixture::new();
    let candidate = fixture.candidate("candidate");

    let native = fixture.store("native");
    fixture.add(&native, "native task");
    let refused = refusal(&fixture.sync(&native, "import", &candidate, true));
    assert_eq!(refused["error"]["code"], "task_sync_native_state");
    assert_eq!(
        refused["error"]["message"],
        "local work input is invalid: task import refuses existing native execution state"
    );
    assert_eq!(
        refused["error"]["details"]["reason"],
        "task import refuses existing native execution state"
    );
    assert_plain_text_kept(
        &fixture.sync(&native, "import", &candidate, false),
        &refused,
    );

    let target = fixture.store("target");
    let foreign = fixture.root.join("foreign");
    fs::create_dir(&foreign).unwrap();
    fs::create_dir(foreign.join("tasks")).unwrap();
    for entry in fs::read_dir(candidate.join("tasks")).unwrap() {
        let entry = entry.unwrap();
        fs::copy(entry.path(), foreign.join("tasks").join(entry.file_name())).unwrap();
    }
    let project = fs::read_to_string(candidate.join("project.json")).unwrap();
    assert!(project.contains(PROJECT));
    fs::write(
        foreign.join("project.json"),
        project.replace(PROJECT, "another-project"),
    )
    .unwrap();
    let refused = refusal(&fixture.sync(&target, "import", &foreign, true));
    assert_eq!(refused["error"]["code"], "task_sync_origin_mismatch");
    assert_plain_text_kept(&fixture.sync(&target, "import", &foreign, false), &refused);

    let malformed = fixture.root.join("malformed");
    fs::create_dir(&malformed).unwrap();
    fs::create_dir(malformed.join("tasks")).unwrap();
    fs::write(malformed.join("project.json"), "{}\n").unwrap();
    let refused = refusal(&fixture.sync(&target, "import", &malformed, true));
    assert_eq!(refused["error"]["code"], "task_sync_invalid_graph");
    assert_plain_text_kept(
        &fixture.sync(&target, "import", &malformed, false),
        &refused,
    );

    let missing = fixture.root.join("missing-tasks");
    fs::create_dir(&missing).unwrap();
    fs::copy(candidate.join("project.json"), missing.join("project.json")).unwrap();
    let refused = refusal(&fixture.sync(&target, "import", &missing, true));
    assert_eq!(
        refused["error"]["code"],
        "task_sync_destination_missing_tasks"
    );
    assert_eq!(
        refused["error"]["message"],
        "local work input is invalid: tasks directory missing"
    );
    assert_plain_text_kept(&fixture.sync(&target, "import", &missing, false), &refused);

    let unknown = fixture.root.join("unknown-entry");
    fs::create_dir(&unknown).unwrap();
    fs::create_dir(unknown.join("tasks")).unwrap();
    fs::copy(candidate.join("project.json"), unknown.join("project.json")).unwrap();
    fs::write(unknown.join("notes.txt"), "not part of the format\n").unwrap();
    let refused = refusal(&fixture.sync(&target, "import", &unknown, true));
    assert_eq!(
        refused["error"]["code"],
        "task_sync_destination_unknown_entry"
    );
    assert_plain_text_kept(&fixture.sync(&target, "import", &unknown, false), &refused);

    // Nothing above was imported, so the candidate still lands, and its
    // exact repeat stays a no-op under --json too.
    for changed in [true, false] {
        let output = fixture.sync(&target, "import", &candidate, true);
        assert!(output.status.success(), "{}", stderr(&output));
        let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(receipt["changed"], changed);
    }
    fixture.add(&target, "local change");
    let refused = refusal(&fixture.sync(&target, "import", &candidate, true));
    assert_eq!(refused["error"]["code"], "task_sync_native_state");
    assert!(refused["error"]["details"].get("remedy").is_none());

    // A refusal outside the task-sync families keeps its generic code, and
    // still carries no work-item remedy.
    let unopted = fixture.root.join("unopted");
    let output = fixture.engram(&unopted).arg("init").output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let refused = refusal(&fixture.sync(&unopted, "import", &candidate, true));
    assert_eq!(refused["error"]["code"], "work_invalid");
    assert!(refused["error"]["details"].get("remedy").is_none());
}

#[test]
fn task_sync_export_refusals_carry_stable_codes_and_json_prints_the_receipt() {
    let fixture = Fixture::new();
    let source = fixture.store("source");
    fixture.add(&source, "shared task");

    let path = fixture.root.join("exported");
    let output = fixture.sync(&source, "export", &path, true);
    assert!(output.status.success(), "{}", stderr(&output));
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["directory"], path.to_str().unwrap());
    assert_eq!(receipt["notes_included"], false);

    fixture.add(&source, "another task");
    let refused = refusal(&fixture.sync(&source, "export", &path, true));
    assert_eq!(refused["error"]["code"], "task_sync_destination_occupied");
    assert_eq!(
        refused["error"]["message"],
        "local work input is invalid: export destination already contains a different candidate"
    );
    assert_plain_text_kept(&fixture.sync(&source, "export", &path, false), &refused);

    // An ordinary file named as a candidate directory is refused on read.
    let file = fixture.root.join("plain-file");
    fs::write(&file, "an ordinary file\n").unwrap();
    let refused = refusal(&fixture.sync(&source, "import", &file, true));
    assert_eq!(
        refused["error"]["code"],
        "task_sync_destination_not_directory"
    );
    assert_eq!(
        refused["error"]["message"],
        "local work input is invalid: task candidate directory must be an ordinary directory"
    );
    assert_plain_text_kept(&fixture.sync(&source, "import", &file, false), &refused);
}
