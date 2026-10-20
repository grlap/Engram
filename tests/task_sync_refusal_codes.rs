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

use engram::task_sync::directory;
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

    /// An import into an empty destination, unless a test selects a basis.
    fn sync_command(&self, home: &Path, operation: &str, path: &Path, json: bool) -> Command {
        let basis: &[&str] = if operation == "import" {
            &["--empty-basis"]
        } else {
            &[]
        };
        self.sync_command_with(home, operation, path, json, basis)
    }

    /// An import against this destination basis file.
    fn import_with_basis(&self, home: &Path, path: &Path, basis: &Path, json: bool) -> Output {
        self.sync_command_with(
            home,
            "import",
            path,
            json,
            &["--basis", basis.to_str().unwrap()],
        )
        .output()
        .unwrap()
    }

    /// A JSON dry run of an import against this destination basis file.
    fn dry_run_with_basis(&self, home: &Path, path: &Path, basis: &Path) -> Output {
        self.sync_command_with(
            home,
            "import",
            path,
            true,
            &["--basis", basis.to_str().unwrap(), "--dry-run"],
        )
        .output()
        .unwrap()
    }

    fn sync_command_with(
        &self,
        home: &Path,
        operation: &str,
        path: &Path,
        json: bool,
        extra: &[&str],
    ) -> Command {
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
            .arg(path)
            .args(extra);
        if json {
            command.arg("--json");
        }
        command
    }

    /// The project document of this store's own folder export.
    fn own_basis(&self, home: &Path, name: &str) -> PathBuf {
        let path = self.root.join(name);
        let output = self.sync(home, "export", &path, false);
        assert!(output.status.success(), "{}", stderr(&output));
        path.join("project.json")
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

    // Against its own basis a native store reaches the native-state refusal;
    // against the empty basis its planning differs first.
    let native = fixture.store("native");
    fixture.add(&native, "native task");
    let native_basis = fixture.own_basis(&native, "native-basis");
    let refused = refusal(&fixture.import_with_basis(&native, &candidate, &native_basis, true));
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
        &fixture.import_with_basis(&native, &candidate, &native_basis, false),
        &refused,
    );
    // A dry run refuses exactly as the real import does.
    assert_eq!(
        refusal(&fixture.dry_run_with_basis(&native, &candidate, &native_basis))["error"],
        refused["error"]
    );
    let refused = refusal(&fixture.sync(&native, "import", &candidate, true));
    assert_eq!(refused["error"]["code"], "task_sync_divergent_planning");

    let target = fixture.store("target");
    let mut folder = directory::read_folder(&candidate).unwrap();
    folder.tree.project.project_id = engram::ProjectId("another-project".into());
    let foreign = fixture.root.join("foreign");
    directory::write_folder(&foreign, &folder.tree, &folder.profile).unwrap();
    let refused = refusal(&fixture.sync(&target, "import", &foreign, true));
    assert_eq!(refused["error"]["code"], "task_sync_origin_mismatch");
    assert_plain_text_kept(&fixture.sync(&target, "import", &foreign, false), &refused);

    // A basis must be this project's, share the folder's note selection, and
    // be well formed; its origin-machine alias is its own.
    let foreign_basis = fixture.root.join("foreign/project.json");
    let refused = refusal(&fixture.import_with_basis(&target, &candidate, &foreign_basis, true));
    assert_eq!(refused["error"]["code"], "task_sync_origin_mismatch");
    let notes_source = fixture.store("notes-source");
    let notes_basis = fixture.root.join("notes-basis");
    let output = fixture
        .sync_command_with(
            &notes_source,
            "export",
            &notes_basis,
            false,
            &["--include-notes", "--origin-machine", "machine-A"],
        )
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let refused = refusal(&fixture.import_with_basis(
        &target,
        &candidate,
        &notes_basis.join("project.json"),
        true,
    ));
    assert_eq!(refused["error"]["code"], "task_sync_divergent_planning");
    let malformed_basis = fixture.root.join("malformed-basis.json");
    fs::write(&malformed_basis, "{}\n").unwrap();
    let refused = refusal(&fixture.import_with_basis(&target, &candidate, &malformed_basis, true));
    assert_eq!(refused["error"]["code"], "task_sync_invalid_graph");

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
    // exact repeat replays the same receipt under --json too.
    let receipts = [0, 1].map(|_| {
        let output = fixture.sync(&target, "import", &candidate, true);
        assert!(output.status.success(), "{}", stderr(&output));
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    });
    assert_eq!(receipts[0]["changed"], true);
    assert_eq!(receipts[1], receipts[0]);

    // A destination whose planning changed after its own export refuses,
    // whichever basis is selected.
    let target_basis = fixture.own_basis(&target, "target-basis");
    let other = fixture.store("other");
    fixture.add(&other, "other task");
    let other_basis = fixture.own_basis(&other, "other-basis");
    fixture.add(&target, "local change");
    for basis in [&target_basis, &other_basis] {
        let refused = refusal(&fixture.import_with_basis(&target, &candidate, basis, true));
        assert_eq!(refused["error"]["code"], "task_sync_divergent_planning");
        assert!(refused["error"]["details"].get("remedy").is_none());
        assert_eq!(
            refusal(&fixture.dry_run_with_basis(&target, &candidate, basis))["error"],
            refused["error"]
        );
    }
    // The already committed intent still replays its first receipt.
    let output = fixture.sync(&target, "import", &candidate, true);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        receipts[0]
    );

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
fn task_sync_resolved_folder_edits_import_and_omitted_members_are_cancelled() {
    let fixture = Fixture::new();
    let source = fixture.store("source");
    fixture.add(&source, "first task");
    fixture.add(&source, "second task");
    let resolved = fixture.root.join("resolved");
    let output = fixture.sync(&source, "export", &resolved, false);
    assert!(output.status.success(), "{}", stderr(&output));

    // A title edited while resolving a Git merge leaves the folder's embedded
    // fingerprint stale; import computes the inventory from the files.
    let folder = directory::read_folder(&resolved).unwrap();
    let edited = &folder.tree.tasks[0];
    let task_path = resolved.join("tasks").join(format!("{}.json", edited.id.0));
    let text = fs::read_to_string(&task_path).unwrap();
    let original = format!("\"title\": \"{}\"", edited.title);
    assert!(text.contains(&original));
    fs::write(
        &task_path,
        text.replace(&original, "\"title\": \"Edited while merging\""),
    )
    .unwrap();
    assert!(directory::read_folder(&resolved).is_err());

    let target = fixture.store("target");
    let output = fixture.sync(&target, "import", &resolved, true);
    assert!(output.status.success(), "{}", stderr(&output));
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["changed"], true);
    let target_basis = fixture.own_basis(&target, "target-basis");
    let imported = directory::read_folder(target_basis.parent().unwrap()).unwrap();
    assert_eq!(imported.tree.tasks.len(), 2);
    assert!(
        imported
            .tree
            .tasks
            .iter()
            .any(|task| task.title == "Edited while merging")
    );

    // The destination's selected basis still binds: the source holds
    // planning, so the empty basis does not match it.
    let refused = refusal(&fixture.sync(&source, "import", &resolved, true));
    assert_eq!(refused["error"]["code"], "task_sync_divergent_planning");

    // A resolved folder that lost a task file reads as a deletion, which an
    // enrolled destination materializes without creating a local run.
    let lost = fixture.root.join("lost");
    fs::create_dir_all(lost.join("tasks")).unwrap();
    fs::copy(&target_basis, lost.join("project.json")).unwrap();
    let kept = format!("{}.json", imported.tree.tasks[1].id.0);
    fs::copy(
        target_basis.parent().unwrap().join("tasks").join(&kept),
        lost.join("tasks").join(&kept),
    )
    .unwrap();
    let removed = fixture.import_with_basis(&target, &lost, &target_basis, true);
    assert!(removed.status.success(), "{}", stderr(&removed));
    let receipt: Value = serde_json::from_slice(&removed.stdout).unwrap();
    assert_eq!(receipt["changed"], true);
    let after = fixture.root.join("after");
    let output = fixture.sync(&target, "export", &after, false);
    assert!(output.status.success(), "{}", stderr(&output));
    let after_tree = directory::read_folder(&after).unwrap().tree;
    assert_eq!(after_tree.tasks, vec![imported.tree.tasks[1].clone()]);
    let store = engram::SqliteStore::open(engram::project_database_path(
        &target,
        &engram::ProjectId(PROJECT.into()),
    ))
    .unwrap();
    let removed_item = store.get_work_item(imported.tree.tasks[0].id).unwrap();
    assert_eq!(removed_item.lifecycle, engram::WorkLifecycle::Cancelled);
    assert_eq!(removed_item.active_run_id, None);
    let connection = rusqlite::Connection::open(engram::project_database_path(
        &target,
        &engram::ProjectId(PROJECT.into()),
    ))
    .unwrap();
    let runs: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM work_runs WHERE work_id = ?1",
            [removed_item.work_id.0.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(runs, 0);
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
    assert_eq!(
        receipt["export_basis"],
        directory::read_folder(&path).unwrap().export_basis.as_str()
    );

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
