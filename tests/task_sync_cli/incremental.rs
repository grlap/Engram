//! The ordinary route between two hosts with their own checkouts: export,
//! commit, merge, commit, import, writing and reading only changed tasks.

use super::*;

#[path = "ordinary.rs"]
mod ordinary;

const PROJECT: &str = "incremental-cli";

struct Host {
    home: std::path::PathBuf,
    marker: std::path::PathBuf,
}

impl Host {
    fn new(root: &Path, name: &str) -> Self {
        let checkout = root.join(name);
        fs::create_dir(&checkout).unwrap();
        let marker = checkout.join(".engram-project");
        fs::write(&marker, format!("{PROJECT}\n")).unwrap();
        data_worktree::initialize(&marker);
        let home = root.join(format!("home-{name}"));
        task_sync_opt_in::enable(&home, PROJECT);
        let output = command(&home, &marker).arg("init").output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Self { home, marker }
    }

    fn checkout(&self) -> &Path {
        self.marker.parent().unwrap()
    }

    fn data(&self) -> std::path::PathBuf {
        self.checkout().join(".engram-data")
    }

    fn folder(&self) -> std::path::PathBuf {
        data_worktree::folder(&self.marker)
    }

    fn sync(&self, operation: &str, extra: &[&str]) -> Output {
        let mut extra = extra.to_vec();
        extra.push("--json");
        sync_output(&self.home, &self.marker, operation, &self.folder(), &extra)
    }

    #[track_caller]
    fn receipt(&self, operation: &str, extra: &[&str]) -> Value {
        let output = self.sync(operation, extra);
        assert!(
            output.status.success(),
            "{operation} {extra:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    #[track_caller]
    fn refusal(&self, operation: &str, extra: &[&str]) -> (String, String) {
        let output = self.sync(operation, extra);
        assert!(!output.status.success(), "{operation} {extra:?}");
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        (
            error["error"]["code"].as_str().unwrap().to_owned(),
            error["error"]["message"].as_str().unwrap().to_owned(),
        )
    }

    fn work(&self, args: &[&str]) {
        work(&self.home, &self.marker, args);
    }

    fn add_ref(&self, title: &str) -> String {
        let output = work(&self.home, &self.marker, &["add", title, "--json"]);
        let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
        receipt["work"]["short_ref"].as_str().unwrap().to_owned()
    }

    fn title(&self, id: engram::WorkId) -> String {
        SqliteStore::open(project_database_path(
            &self.home,
            &ProjectId(PROJECT.into()),
        ))
        .unwrap()
        .get_work_item(id)
        .unwrap()
        .title
    }

    fn git(&self, args: &[&str]) {
        data_worktree::git(&self.data(), &identity(args));
    }

    fn commit(&self, subject: &str) {
        self.git(&["add", "-A", "--", ".engram"]);
        self.git(&["commit", "-m", subject]);
    }

    #[track_caller]
    fn commit_and_confirm_export(&self, args: &[&str], subject: &str) {
        assert_export_commit_first(&self.sync("export", args));
        self.commit(subject);
        let confirmed = self.receipt("export", args);
        assert_eq!(confirmed["route"], "unchanged");
        assert_eq!(confirmed["written"], 0);
    }

    /// The data branch's current commit.
    fn head(&self) -> String {
        let data = self.data();
        let output = Command::new("git")
            .current_dir(&data)
            .args(["rev-parse", "HEAD"])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", data.join(".gitconfig-absent"))
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    /// The task file this host's folder holds for `id`.
    fn task_file(&self, id: engram::WorkId) -> Value {
        serde_json::from_slice(
            &fs::read(self.folder().join("tasks").join(format!("{}.json", id.0))).unwrap(),
        )
        .unwrap()
    }

    /// Bring in the other host's data branch: adopt it on an unborn branch,
    /// fast-forward to it, or merge it. A merge keeps this host's envelope,
    /// which no import trusts; disjoint task files merge on their own.
    fn merge_from(&self, other: &Host) {
        let source = other.checkout().to_str().unwrap().to_owned();
        self.git(&["fetch", &source, "engram-data"]);
        let data = self.data();
        let born = Command::new("git")
            .current_dir(&data)
            .args(["rev-parse", "--verify", "--quiet", "HEAD"])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", data.join(".gitconfig-absent"))
            .output()
            .unwrap()
            .status
            .success();
        if !born {
            self.git(&["reset", "--hard", "FETCH_HEAD"]);
            return;
        }
        let ancestor = Command::new("git")
            .current_dir(&data)
            .args(["merge-base", "--is-ancestor", "HEAD", "FETCH_HEAD"])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", data.join(".gitconfig-absent"))
            .output()
            .unwrap()
            .status
            .success();
        if ancestor {
            self.git(&["merge", "--ff-only", "FETCH_HEAD"]);
            return;
        }
        // The envelope conflicts: both hosts rewrote it.
        let merge = Command::new("git")
            .current_dir(&data)
            .args(identity(&["merge", "--no-ff", "--no-commit", "FETCH_HEAD"]))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", data.join(".gitconfig-absent"))
            .output()
            .unwrap();
        assert!(
            merge.status.code().is_some_and(|code| code <= 1),
            "{}",
            String::from_utf8_lossy(&merge.stderr)
        );
        self.git(&["checkout", "--ours", "--", ".engram/project.json"]);
        self.git(&["add", "-A", "--", ".engram"]);
        self.git(&["commit", "-m", "Merge the other host's planning"]);
    }
}

fn identity<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut all = vec![
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
    ];
    all.extend_from_slice(args);
    all
}

/// Two hosts that share notes under their own aliases, enrolled on host A's
/// published task, and that task's id.
fn noted_exchange(root: &Path) -> (Host, Host, engram::WorkId) {
    let a = Host::new(root, "a");
    let b = Host::new(root, "b");
    a.work(&["add", "Shared"]);
    assert_eq!(a.receipt("export", &NOTES_A)["route"], "bootstrap");
    let a_basis = root.join("a-basis.json");
    fs::copy(a.folder().join("project.json"), &a_basis).unwrap();
    a.commit("Publish planning from A");
    let id = directory::read_folder(&a.folder()).unwrap().tree.tasks[0].id;
    b.merge_from(&a);
    b.receipt("import", &["--empty-basis"]);
    a.receipt("import", &["--basis", a_basis.to_str().unwrap()]);
    (a, b, id)
}

const NOTES_A: [&str; 3] = ["--include-notes", "--origin-machine", "host-a"];
const NOTES_B: [&str; 3] = ["--include-notes", "--origin-machine", "host-b"];

#[test]
fn task_sync_cli_a_renewed_claim_takes_another_hosts_note_without_divergence() {
    let fixture = test_support::temp_home().unwrap();
    let (a, b, id) = noted_exchange(fixture.path());
    let reference = id.0.to_string();
    a.work(&["claim", &reference]);
    assert_eq!(a.receipt("export", &NOTES_A)["written"], 1);
    a.commit("Claim on A");
    b.merge_from(&a);
    assert_eq!(b.receipt("import", &[])["route"], "incremental");
    // B notes the task without changing what governs it.
    b.work(&["claim", &reference]);
    b.work(&["note", "Seen from B"]);
    assert_eq!(b.receipt("export", &NOTES_B)["written"], 1);
    b.commit("Note on B");
    // A renews its claim: execution, not planning.
    a.work(&["claim", &reference]);
    a.merge_from(&b);
    let imported = a.receipt("import", &[]);
    assert_eq!(imported["route"], "incremental");
    assert!(
        imported["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|task| task["status"] == "applied"),
        "{imported}"
    );
}

#[test]
fn task_sync_cli_another_hosts_child_of_a_task_whose_note_export_was_discarded_is_admitted() {
    let fixture = test_support::temp_home().unwrap();
    let (a, b, _) = noted_exchange(fixture.path());
    // A publishes a new task and, before importing its own commit, notes it
    // and exports again; that export is discarded.
    a.work(&["add", "Parent on A"]);
    assert_eq!(a.receipt("export", &NOTES_A)["written"], 1);
    a.commit("Add a parent on A");
    let parent = directory::read_folder(&a.folder())
        .unwrap()
        .tree
        .tasks
        .into_iter()
        .find(|task| task.title == "Parent on A")
        .unwrap()
        .id;
    let reference = parent.0.to_string();
    a.work(&["claim", &reference]);
    a.work(&["note", "Started on A"]);
    assert_eq!(a.receipt("export", &NOTES_A)["written"], 1);
    a.git(&["checkout", "--", ".engram"]);
    // A releases its claim, which would otherwise hold a child under it.
    a.work(&[
        "update",
        &reference,
        "--release",
        "--reason",
        "Hand the task on",
    ]);
    // B adds a child of that task.
    b.merge_from(&a);
    assert_eq!(b.receipt("import", &[])["route"], "incremental");
    b.work(&["add", "Child on B", "--under", &reference]);
    assert!(b.receipt("export", &NOTES_B)["written"].as_u64().unwrap() >= 1);
    b.commit("Add a child on B");
    // A's import reads its own published parent as shared and admits the
    // child; the discarded note stays owed and the next export writes it.
    a.merge_from(&b);
    let imported = a.receipt("import", &[]);
    assert_eq!(imported["route"], "incremental");
    assert!(
        imported["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|task| task["status"] == "applied"),
        "{imported}"
    );
    assert!(
        directory::read_folder(&a.folder())
            .unwrap()
            .tree
            .tasks
            .iter()
            .any(|task| task.title == "Child on B")
    );
    assert!(a.receipt("export", &NOTES_A)["written"].as_u64().unwrap() >= 1);
    let task = a.task_file(parent);
    assert!(
        task["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|note| note["body"] == "Started on A"),
        "{task}"
    );
}

#[test]
fn task_sync_cli_a_discarded_note_on_a_new_child_admits_another_hosts_edit_of_its_new_parent() {
    let fixture = test_support::temp_home().unwrap();
    let (a, b, _) = noted_exchange(fixture.path());
    let id_of = |host: &Host, title: &str| {
        directory::read_folder(&host.folder())
            .unwrap()
            .tree
            .tasks
            .into_iter()
            .find(|task| task.title == title)
            .unwrap()
            .id
    };
    // A publishes a new parent and its new child in one commit, without
    // importing it.
    let parent_ref = a.add_ref("Parent on A");
    a.work(&["add", "Child on A", "--under", &parent_ref]);
    assert_eq!(a.receipt("export", &NOTES_A)["written"], 2);
    a.commit_and_confirm_export(&NOTES_A, "Add a parent and a child on A");
    let parent = id_of(&a, "Parent on A");
    let child = id_of(&a, "Child on A");
    let child_ref = child.0.to_string();
    // A notes the child and exports again; that export is discarded.
    a.work(&["claim", &child_ref]);
    a.work(&["note", "Started on A"]);
    assert!(a.receipt("export", &NOTES_A)["written"].as_u64().unwrap() >= 1);
    a.git(&["checkout", "--", ".engram"]);
    a.work(&[
        "update",
        &child_ref,
        "--release",
        "--reason",
        "Hand the task on",
    ]);
    // B renames the new parent and leaves the child as A published it.
    b.merge_from(&a);
    assert_eq!(b.receipt("import", &[])["route"], "incremental");
    b.work(&[
        "update",
        &parent_ref,
        "--title",
        "Parent on A, renamed on B",
    ]);
    assert!(b.receipt("export", &NOTES_B)["written"].as_u64().unwrap() >= 1);
    b.commit("Rename the parent on B");
    // The child joins the baseline only beside its parent, so neither a dry
    // run, the import nor a retry meets a baseline missing that parent.
    a.merge_from(&b);
    let dry = a.receipt("import", &["--dry-run"]);
    assert_eq!(dry["route"], "incremental");
    let imported = a.receipt("import", &[]);
    assert_eq!(imported["route"], "incremental");
    assert!(
        imported["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|task| task["status"] == "applied"),
        "{imported}"
    );
    assert_eq!(a.title(parent), "Parent on A, renamed on B");
    a.receipt("import", &[]);
    // The discarded note stays owed, and the next export writes it.
    assert!(a.receipt("export", &NOTES_A)["written"].as_u64().unwrap() >= 1);
    let task = a.task_file(child);
    assert!(
        task["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|note| note["body"] == "Started on A"),
        "{task}"
    );
}

#[test]
fn task_sync_cli_another_hosts_child_of_a_new_child_with_a_discarded_note_is_admitted() {
    let fixture = test_support::temp_home().unwrap();
    let (a, b, _) = noted_exchange(fixture.path());
    let id_of = |host: &Host, title: &str| {
        directory::read_folder(&host.folder())
            .unwrap()
            .tree
            .tasks
            .into_iter()
            .find(|task| task.title == title)
            .unwrap()
            .id
    };
    // A publishes a new parent and its new child in one commit, without
    // importing it, then notes the child in an export that is discarded.
    let parent_ref = a.add_ref("Parent on A");
    a.work(&["add", "Child on A", "--under", &parent_ref]);
    assert_eq!(a.receipt("export", &NOTES_A)["written"], 2);
    a.commit_and_confirm_export(&NOTES_A, "Add a parent and a child on A");
    let parent = id_of(&a, "Parent on A");
    let child = id_of(&a, "Child on A");
    let child_ref = child.0.to_string();
    a.work(&["claim", &child_ref]);
    a.work(&["note", "Started on A"]);
    assert!(a.receipt("export", &NOTES_A)["written"].as_u64().unwrap() >= 1);
    a.git(&["checkout", "--", ".engram"]);
    a.work(&[
        "update",
        &child_ref,
        "--release",
        "--reason",
        "Hand the task on",
    ]);
    // B renames the new parent and adds a child under the new child.
    b.merge_from(&a);
    assert_eq!(b.receipt("import", &[])["route"], "incremental");
    b.work(&[
        "update",
        &parent_ref,
        "--title",
        "Parent on A, renamed on B",
    ]);
    b.work(&["add", "Grandchild on B", "--under", &child_ref]);
    assert!(b.receipt("export", &NOTES_B)["written"].as_u64().unwrap() >= 1);
    b.commit("Rename the parent and add a grandchild on B");
    let grandchild = id_of(&b, "Grandchild on B");
    // Admission judges against everything A published, so the grandchild
    // finds its parent in a dry run, the import and a retry alike.
    a.merge_from(&b);
    let dry = a.receipt("import", &["--dry-run"]);
    assert_eq!(dry["route"], "incremental");
    let imported = a.receipt("import", &[]);
    assert_eq!(imported["route"], "incremental");
    assert!(
        imported["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|task| task["status"] == "applied"),
        "{imported}"
    );
    assert_eq!(a.title(parent), "Parent on A, renamed on B");
    assert_eq!(a.title(grandchild), "Grandchild on B");
    a.receipt("import", &[]);
    // The discarded note stays owed, and the next export writes it.
    assert!(a.receipt("export", &NOTES_A)["written"].as_u64().unwrap() >= 1);
    let task = a.task_file(child);
    assert!(
        task["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|note| note["body"] == "Started on A"),
        "{task}"
    );
}

#[test]
fn task_sync_cli_a_discarded_note_beside_a_reversed_prerequisite_admits_another_hosts_edit() {
    let fixture = test_support::temp_home().unwrap();
    let (a, b, _) = noted_exchange(fixture.path());
    let id_of = |host: &Host, title: &str| {
        directory::read_folder(&host.folder())
            .unwrap()
            .tree
            .tasks
            .into_iter()
            .find(|task| task.title == title)
            .unwrap()
            .id
    };
    // Both hosts hold First waiting for Second.
    let first_ref = a.add_ref("First");
    let second_ref = a.add_ref("Second");
    a.work(&["update", &first_ref, "--after", &second_ref]);
    assert_eq!(a.receipt("export", &NOTES_A)["written"], 2);
    a.commit_and_confirm_export(&NOTES_A, "First waits for Second on A");
    let first = id_of(&a, "First");
    let second = id_of(&a, "Second");
    b.merge_from(&a);
    assert_eq!(b.receipt("import", &[])["route"], "incremental");
    // A starts on Second while it waits for nothing, reverses the order and
    // publishes it, then notes Second in an export that is discarded.
    a.work(&["claim", &second_ref]);
    a.work(&["update", &first_ref, "--drop-after", &second_ref]);
    a.work(&["update", &second_ref, "--after", &first_ref]);
    assert!(a.receipt("export", &NOTES_A)["written"].as_u64().unwrap() >= 2);
    a.commit("Second waits for First on A");
    a.work(&["note", &second_ref, "Started on A"]);
    assert!(a.receipt("export", &NOTES_A)["written"].as_u64().unwrap() >= 1);
    a.git(&["checkout", "--", ".engram"]);
    a.work(&[
        "update",
        &second_ref,
        "--release",
        "--reason",
        "Hand the task on",
    ]);
    // B takes the new order and renames First.
    b.merge_from(&a);
    assert_eq!(b.receipt("import", &[])["route"], "incremental");
    b.work(&["update", &first_ref, "--title", "First, renamed on B"]);
    assert_eq!(b.receipt("export", &NOTES_B)["written"], 1);
    b.commit("Rename First on B");
    // Admission judges against the order A published, never beside the old
    // one, so no dry run, import or retry meets a cycle.
    a.merge_from(&b);
    let dry = a.receipt("import", &["--dry-run"]);
    assert_eq!(dry["route"], "incremental");
    let imported = a.receipt("import", &[]);
    assert_eq!(imported["route"], "incremental");
    assert!(
        imported["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|task| task["status"] == "applied"),
        "{imported}"
    );
    assert_eq!(a.title(first), "First, renamed on B");
    a.receipt("import", &[]);
    // The discarded note stays owed, and the next export writes it.
    assert!(a.receipt("export", &NOTES_A)["written"].as_u64().unwrap() >= 1);
    let task = a.task_file(second);
    assert!(
        task["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|note| note["body"] == "Started on A"),
        "{task}"
    );
}

#[test]
fn task_sync_cli_another_hosts_revert_of_a_published_edit_keeps_later_imports_admitted() {
    let fixture = test_support::temp_home().unwrap();
    let (a, b, id) = noted_exchange(fixture.path());
    let reference = id.0.to_string();
    // B, which imported A's task, publishes an edit of it.
    b.work(&["update", &reference, "--title", "Shared, renamed on B"]);
    assert_eq!(b.receipt("export", &NOTES_B)["written"], 1);
    b.commit("Rename on B");
    // A takes the edit and returns the task to its earlier bytes.
    a.merge_from(&b);
    assert_eq!(a.receipt("import", &[])["route"], "incremental");
    a.work(&["update", &reference, "--title", "Shared"]);
    assert_eq!(a.receipt("export", &NOTES_A)["written"], 1);
    a.commit("Rename back on A");
    // B's import takes the revert over the edit it published.
    b.merge_from(&a);
    let imported = b.receipt("import", &[]);
    assert_eq!(imported["route"], "incremental");
    assert!(
        imported["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|task| task["status"] == "applied"),
        "{imported}"
    );
    assert_eq!(b.title(id), "Shared");
    // A later edit of the same task still applies on B.
    a.work(&[
        "update",
        &reference,
        "--title",
        "Shared, renamed again on A",
    ]);
    assert_eq!(a.receipt("export", &NOTES_A)["written"], 1);
    a.commit("Rename again on A");
    b.merge_from(&a);
    let again = b.receipt("import", &[]);
    assert_eq!(again["route"], "incremental");
    assert!(
        again["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|task| task["status"] == "applied"),
        "{again}"
    );
    assert_eq!(b.title(id), "Shared, renamed again on A");
}

#[test]
fn task_sync_cli_a_note_on_a_held_task_stays_owed_and_never_conflicts() {
    let fixture = test_support::temp_home().unwrap();
    let (a, b, id) = noted_exchange(fixture.path());
    let reference = id.0.to_string();
    // B edits the task while A works on it: A's import holds the edit.
    b.work(&["update", &reference, "--title", "Shared, edited on B"]);
    assert_eq!(b.receipt("export", &NOTES_B)["written"], 1);
    b.commit("Edit on B");
    a.work(&["claim", &reference]);
    a.merge_from(&b);
    let held = a.sync("import", &[]);
    assert_eq!(
        held.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&held.stderr)
    );
    // A notes the task it holds: no conflict with the held edit, neither for
    // the export nor for the next import, which holds it for its run alone.
    a.work(&["note", "Progress on A"]);
    let exported = a.sync("export", &NOTES_A);
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    let again = a.sync("import", &[]);
    assert_eq!(
        again.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    let again: Value = serde_json::from_slice(&again.stdout).unwrap();
    let entry = again["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["task"] == reference)
        .unwrap()
        .clone();
    assert_eq!(
        (entry["status"].clone(), entry["reason"].clone()),
        ("held".into(), "active_run".into()),
        "{again}"
    );
    // Released, the next import applies B's edit, and the next export
    // publishes A's note beside it.
    a.work(&[
        "update",
        &reference,
        "--release",
        "--reason",
        "Yield to the edit from B",
    ]);
    assert_eq!(a.receipt("import", &[])["route"], "incremental");
    assert_eq!(a.title(id), "Shared, edited on B");
    assert_eq!(a.receipt("export", &NOTES_A)["written"], 1);
    let task = a.task_file(id);
    assert_eq!(task["title"], "Shared, edited on B");
    assert!(
        task["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|note| note["body"] == "Progress on A"),
        "{task}"
    );
}

#[test]
fn task_sync_cli_ordinary_exchange_writes_and_reads_only_changed_tasks() {
    let fixture = test_support::temp_home().unwrap();
    let root = fixture.path();
    let a = Host::new(root, "a");
    let b = Host::new(root, "b");
    for title in ["First", "Second", "Third", "Fourth"] {
        a.work(&["add", title]);
    }

    // A writes its complete folder once and keeps its own basis.
    let bootstrap = a.receipt("export", &[]);
    assert_eq!(bootstrap["route"], "bootstrap");
    assert_eq!(
        (&bootstrap["written"], &bootstrap["task_count"]),
        (&4.into(), &4.into())
    );
    let a_basis = root.join("a-basis.json");
    fs::copy(a.folder().join("project.json"), &a_basis).unwrap();
    a.commit("Publish planning from A");
    let tasks = directory::read_folder(&a.folder()).unwrap().tree.tasks;
    let id = |title: &str| tasks.iter().find(|task| task.title == title).unwrap().id;
    let reference = |title: &str| id(title).0.to_string();

    // B's first import names its basis; without one it is refused, by name.
    b.merge_from(&a);
    let (code, message) = b.refusal("import", &[]);
    assert_eq!(code, "task_sync_incremental_basis_unavailable");
    assert!(message.contains("--empty-basis"), "{message}");
    let enrolled = b.receipt("import", &["--empty-basis"]);
    assert_eq!(
        (&enrolled["route"], &enrolled["changed"]),
        (&"enrol".into(), &true.into())
    );

    // B edits one task; its export writes that one file.
    b.work(&[
        "update",
        &reference("Second"),
        "--title",
        "Second, edited on B",
    ]);
    let exported = b.receipt("export", &[]);
    assert_eq!(exported["route"], "incremental");
    assert_eq!(
        (
            &exported["written"],
            &exported["removed"],
            &exported["task_count"]
        ),
        (&1.into(), &0.into(), &4.into())
    );
    assert!(exported.get("export_basis").is_none());
    b.commit("Edit Second on B");

    // A enrols from the merged folder against its own export.
    a.merge_from(&b);
    let enrolled = a.receipt("import", &["--basis", a_basis.to_str().unwrap()]);
    assert_eq!(
        (&enrolled["route"], &enrolled["changed"]),
        (&"enrol".into(), &true.into())
    );
    assert_eq!(a.title(id("Second")), "Second, edited on B");

    // Both hosts edit different tasks at once. Each export writes one file,
    // and a merge of the two branches carries both edits.
    a.work(&[
        "update",
        &reference("First"),
        "--title",
        "First, edited on A",
    ]);
    b.work(&[
        "update",
        &reference("Third"),
        "--title",
        "Third, edited on B",
    ]);
    for host in [&a, &b] {
        let exported = host.receipt("export", &[]);
        assert_eq!(exported["route"], "incremental");
        assert_eq!(exported["written"], 1);
        host.commit("Edit one task");
    }
    b.merge_from(&a);
    assert_eq!(b.receipt("import", &[])["route"], "incremental");
    assert_eq!(b.title(id("First")), "First, edited on A");
    assert_eq!(b.receipt("import", &[])["route"], "unchanged");
    a.merge_from(&b);
    assert_eq!(a.receipt("import", &[])["route"], "incremental");
    assert_eq!(a.title(id("Third")), "Third, edited on B");
    // Nothing changed on A since that import.
    let unchanged = a.receipt("export", &[]);
    assert_eq!(
        (&unchanged["route"], &unchanged["written"]),
        (&"unchanged".into(), &0.into())
    );

    // An incremental import reads a commit, never loose files.
    a.work(&[
        "update",
        &reference("Fourth"),
        "--title",
        "Fourth, edited on A",
    ]);
    assert_eq!(a.receipt("export", &[])["written"], 1);
    let (code, message) = a.refusal("import", &[]);
    assert_eq!(code, "task_sync_commit_context_required");
    assert!(message.contains("--rebuild"), "{message}");
    a.commit("Edit Fourth on A");

    // A force-moved data branch is refused by name. The complete route
    // imports it again against this host's own fresh export.
    b.git(&["commit", "--amend", "-m", "Rewrite the merged planning"]);
    let (code, message) = b.refusal("import", &[]);
    assert_eq!(code, "task_sync_source_history_diverged");
    assert!(message.contains("--rebuild"), "{message}");
    let rebuilt = b.receipt("export", &["--rebuild"]);
    assert_eq!(rebuilt["route"], "rebuild");
    assert_eq!(rebuilt["task_count"], 4);
    assert!(rebuilt["export_basis"].is_string());
    let b_basis = root.join("b-basis.json");
    fs::copy(b.folder().join("project.json"), &b_basis).unwrap();
    b.git(&["checkout", "--", ".engram"]);
    let reimported = b.receipt(
        "import",
        &["--rebuild", "--basis", b_basis.to_str().unwrap()],
    );
    assert_eq!(reimported["route"], "rebuild");
    assert_eq!(b.receipt("import", &[])["route"], "unchanged");
    let store =
        SqliteStore::open(project_database_path(&b.home, &ProjectId(PROJECT.into()))).unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn task_sync_cli_ordinary_exchange_retries_held_tasks_keeps_local_edits_and_refuses_unimported_commits()
 {
    let fixture = test_support::temp_home().unwrap();
    let root = fixture.path();
    let a = Host::new(root, "a");
    let b = Host::new(root, "b");
    for title in ["First", "Second", "Third"] {
        a.work(&["add", title]);
    }
    assert_eq!(a.receipt("export", &[])["route"], "bootstrap");
    let a_basis = root.join("a-basis.json");
    fs::copy(a.folder().join("project.json"), &a_basis).unwrap();
    a.commit("Publish planning from A");
    let tasks = directory::read_folder(&a.folder()).unwrap().tree.tasks;
    let id = |title: &str| tasks.iter().find(|task| task.title == title).unwrap().id;
    let reference = |title: &str| id(title).0.to_string();
    b.merge_from(&a);
    b.receipt("import", &["--empty-basis"]);

    // B edits a task A is working on: A's first import holds that edit.
    b.work(&[
        "update",
        &reference("First"),
        "--title",
        "First, edited on B",
    ]);
    assert_eq!(b.receipt("export", &[])["written"], 1);
    b.commit("Edit First on B");
    a.work(&["claim", &reference("First")]);
    a.merge_from(&b);
    let partial = a.sync("import", &["--basis", a_basis.to_str().unwrap()]);
    assert_eq!(
        partial.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&partial.stderr)
    );
    let partial: Value = serde_json::from_slice(&partial.stdout).unwrap();
    let held = partial["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["task"] == reference("First"))
        .unwrap();
    assert_eq!(held["status"], "held");
    assert_eq!(a.title(id("First")), "First");

    // That import applied nothing, so it recorded neither a baseline nor a
    // head: the next import is the same complete one, named by its basis.
    let (code, message) = a.refusal("import", &[]);
    assert_eq!(code, "task_sync_incremental_basis_unavailable");
    assert!(message.contains("--basis"), "{message}");
    a.work(&[
        "update",
        &reference("First"),
        "--release",
        "--reason",
        "Yield to the edit from B",
    ]);
    // A starts on another task before enrolling.
    a.work(&["claim", &reference("Second")]);
    let enrolled = a.receipt("import", &["--basis", a_basis.to_str().unwrap()]);
    assert_eq!(enrolled["route"], "enrol");
    assert_eq!(a.title(id("First")), "First, edited on B");

    // An incremental import holds B's edit to the task A is working on.
    // Released, the next import reads it again at the same commit and
    // applies it: a claim and its release are not planning changes.
    b.work(&[
        "update",
        &reference("Second"),
        "--title",
        "Second, edited on B",
    ]);
    assert_eq!(b.receipt("export", &[])["written"], 1);
    b.commit("Edit Second on B");
    a.merge_from(&b);
    let partial = a.sync("import", &[]);
    assert_eq!(
        partial.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&partial.stderr)
    );
    let partial: Value = serde_json::from_slice(&partial.stdout).unwrap();
    assert_eq!(partial["route"], "incremental");
    let held = partial["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["task"] == reference("Second"))
        .unwrap();
    assert_eq!(held["status"], "held");
    assert_eq!(a.title(id("Second")), "Second");
    a.work(&[
        "update",
        &reference("Second"),
        "--release",
        "--reason",
        "Yield to the edit from B",
    ]);
    assert_eq!(a.receipt("import", &[])["route"], "incremental");
    assert_eq!(a.title(id("Second")), "Second, edited on B");

    // A local edit the next import passes over stays owed to the export.
    a.work(&[
        "update",
        &reference("Second"),
        "--title",
        "Second, edited on A",
    ]);
    b.work(&[
        "update",
        &reference("Third"),
        "--title",
        "Third, edited on B",
    ]);
    assert_eq!(b.receipt("export", &[])["written"], 1);
    b.commit("Edit Third on B");
    a.merge_from(&b);
    assert_eq!(a.receipt("import", &[])["route"], "incremental");
    assert_eq!(a.title(id("Third")), "Third, edited on B");
    let exported = a.receipt("export", &[]);
    assert_eq!(
        (&exported["route"], &exported["written"]),
        (&"incremental".into(), &1.into())
    );
    a.commit("Edit Second on A");
    b.merge_from(&a);
    assert_eq!(b.receipt("import", &[])["route"], "incremental");
    assert_eq!(b.title(id("Second")), "Second, edited on A");

    // A commit another host added is imported before this host exports over
    // it, whatever the folder's envelope says.
    b.work(&[
        "update",
        &reference("Third"),
        "--title",
        "Third, edited again on B",
    ]);
    assert_eq!(b.receipt("export", &[])["written"], 1);
    b.commit("Edit Third again on B");
    a.merge_from(&b);
    a.work(&[
        "update",
        &reference("First"),
        "--title",
        "First, edited on A",
    ]);
    let (code, message) = a.refusal("export", &[]);
    assert_eq!(code, "task_sync_incremental_basis_unavailable");
    assert!(
        message.contains("does not match the admitted export basis")
            && message.contains("run `engram task-sync import` first")
            && message.contains("`engram task-sync export --rebuild`"),
        "{message}"
    );
    assert_eq!(a.receipt("import", &[])["route"], "incremental");
    assert_eq!(a.title(id("Third")), "Third, edited again on B");
    assert_eq!(a.title(id("First")), "First, edited on A");
    let exported = a.receipt("export", &[]);
    assert_eq!(
        (&exported["route"], &exported["written"]),
        (&"incremental".into(), &1.into())
    );
    a.commit("Edit First on A");

    // This host's own incremental export is a basis for the complete route
    // while nothing changed since it.
    let own = root.join("a-incremental-basis.json");
    fs::copy(a.folder().join("project.json"), &own).unwrap();
    let rebuilt = a.receipt("import", &["--rebuild", "--basis", own.to_str().unwrap()]);
    assert_eq!(rebuilt["route"], "rebuild");
    assert_eq!(a.receipt("import", &[])["route"], "unchanged");
    for host in [&a, &b] {
        let store = SqliteStore::open(project_database_path(
            &host.home,
            &ProjectId(PROJECT.into()),
        ))
        .unwrap();
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

#[test]
fn task_sync_cli_ordinary_exchange_names_an_exit_for_every_conflict() {
    let fixture = test_support::temp_home().unwrap();
    let root = fixture.path();
    let a = Host::new(root, "a");
    let b = Host::new(root, "b");
    for title in ["Parent", "Other"] {
        a.work(&["add", title]);
    }
    assert_eq!(a.receipt("export", &[])["route"], "bootstrap");
    let a_basis = root.join("a-basis.json");
    fs::copy(a.folder().join("project.json"), &a_basis).unwrap();
    a.commit("Publish planning from A");
    assert_eq!(
        a.receipt("import", &["--basis", a_basis.to_str().unwrap()])["route"],
        "enrol"
    );
    let first = directory::read_folder(&a.folder()).unwrap().tree.tasks;
    let parent = first.iter().find(|task| task.title == "Parent").unwrap().id;
    let other = first.iter().find(|task| task.title == "Other").unwrap().id;
    a.work(&["add", "Child", "--under", &parent.0.to_string()]);
    // Adding a child also moves other tasks in the project feed. A file
    // whose bytes stay the same is not rewritten, and the import of this
    // commit relies on that.
    let added = a.receipt("export", &[]);
    assert_eq!(
        (&added["route"], &added["written"]),
        (&"incremental".into(), &1.into())
    );
    let child = directory::read_folder(&a.folder())
        .unwrap()
        .tree
        .tasks
        .into_iter()
        .find(|task| task.title == "Child")
        .unwrap()
        .id;
    a.commit("Add Child on A");
    let confirmed = a.receipt("import", &[]);
    assert_eq!(confirmed["route"], "unchanged");
    assert_eq!(confirmed["changed"], false);
    let reference = |id: engram::WorkId| id.0.to_string();

    // A first export over another host's planning is refused by name: the
    // folder is imported first, or replaced explicitly.
    b.merge_from(&a);
    let (code, message) = b.refusal("export", &[]);
    assert_eq!(code, "task_sync_destination_occupied");
    assert!(message.contains("--empty-basis"), "{message}");
    assert!(message.contains("--rebuild"), "{message}");
    assert_eq!(
        directory::read_folder(&b.folder())
            .unwrap()
            .tree
            .tasks
            .len(),
        3
    );
    b.receipt("import", &["--empty-basis"]);

    // A changes Parent and merges B's change to its Child before exporting:
    // the import refuses and names an exit that only moves the data branch
    // forward, and that exit works.
    a.work(&[
        "update",
        &reference(parent),
        "--title",
        "Parent, edited on A",
    ]);
    b.work(&["update", &reference(child), "--title", "Child, edited on B"]);
    assert_eq!(b.receipt("export", &[])["written"], 1);
    b.commit("Edit Child on B");
    let before_merge = a.head();
    a.merge_from(&b);
    let (code, message) = a.refusal("import", &[]);
    assert_eq!(code, "task_sync_divergent_planning");
    let set_aside = format!("--source={before_merge}");
    assert!(message.contains(&set_aside), "{message}");
    assert!(
        message.contains("and commit that; import, then export and commit;"),
        "{message}"
    );
    assert!(message.contains("git revert"), "{message}");
    let merged = a.head();
    a.git(&[
        "restore",
        &set_aside,
        "--staged",
        "--worktree",
        "--",
        ".engram/tasks",
    ]);
    a.git(&["commit", "-m", "Set B's planning aside"]);
    let aside = a.head();
    assert_eq!(a.receipt("import", &[])["route"], "incremental");
    assert_eq!(a.title(child), "Child");
    assert_eq!(a.title(parent), "Parent, edited on A");
    // A starts on Other before this export, which takes its feed position
    // without rewriting its unchanged file.
    a.work(&["claim", &reference(other)]);
    assert_eq!(a.receipt("export", &[])["written"], 1);
    a.commit("Edit Parent on A");
    a.git(&["revert", "--no-edit", &aside]);
    assert_eq!(a.receipt("import", &[])["route"], "incremental");
    // The branch only moved forward.
    a.git(&["merge-base", "--is-ancestor", &merged, "HEAD"]);
    assert_eq!(a.title(child), "Child, edited on B");
    assert_eq!(a.title(parent), "Parent, edited on A");

    // B's edit to a task A works on is held. A then changes it too: the
    // import holds it as a conflict and goes ahead, the export refuses it
    // by name, and a rebuild writes A's version over the held one and says
    // so.
    b.merge_from(&a);
    assert_eq!(b.receipt("import", &[])["route"], "incremental");
    b.work(&["update", &reference(other), "--title", "Other, edited on B"]);
    assert_eq!(b.receipt("export", &[])["written"], 1);
    b.commit("Edit Other on B");
    a.merge_from(&b);
    let held = a.sync("import", &[]);
    assert_eq!(held.status.code(), Some(3));
    a.work(&["update", &reference(other), "--title", "Other, edited on A"]);
    let conflicted = a.sync("import", &[]);
    assert_eq!(
        conflicted.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&conflicted.stderr)
    );
    let conflicted: Value = serde_json::from_slice(&conflicted.stdout).unwrap();
    let task = conflicted["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["task"] == reference(other))
        .unwrap();
    assert_eq!(
        (&task["status"], &task["reason"]),
        (&"held".into(), &"local_planning_changed".into())
    );
    let (code, message) = a.refusal("export", &[]);
    assert_eq!(code, "task_sync_import_conflict");
    assert!(message.contains("--rebuild"), "{message}");
    let rebuilt = a.receipt("export", &["--rebuild"]);
    assert_eq!(
        (&rebuilt["route"], &rebuilt["superseded"]),
        (&"rebuild".into(), &serde_json::json!([reference(other)]))
    );
    assert_eq!(a.task_file(other)["title"], "Other, edited on A");
    a.commit("Keep A's version of Other");
    a.receipt("import", &[]);
    b.merge_from(&a);
    assert_eq!(b.receipt("import", &[])["route"], "incremental");
    assert_eq!(b.title(other), "Other, edited on A");

    // An exported change discarded before its commit stays owed through the
    // next import, and the export after it writes the change again.
    a.work(&[
        "update",
        &reference(other),
        "--title",
        "Other, edited again on A",
    ]);
    assert_eq!(a.receipt("export", &[])["written"], 1);
    a.git(&["checkout", "--", ".engram"]);
    b.work(&[
        "update",
        &reference(child),
        "--title",
        "Child, edited again on B",
    ]);
    assert_eq!(b.receipt("export", &[])["written"], 1);
    b.commit("Edit Child again on B");
    a.merge_from(&b);
    assert_eq!(a.receipt("import", &[])["route"], "incremental");
    let exported = a.receipt("export", &[]);
    assert_eq!(
        (&exported["route"], &exported["written"]),
        (&"incremental".into(), &1.into())
    );
    assert_eq!(a.task_file(other)["title"], "Other, edited again on A");
    for host in [&a, &b] {
        let store = SqliteStore::open(project_database_path(
            &host.home,
            &ProjectId(PROJECT.into()),
        ))
        .unwrap();
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

/// The id of the task titled `title` in the folder `host` last exported.
fn exported_id(host: &Host, title: &str) -> engram::WorkId {
    directory::read_folder(&host.folder())
        .unwrap()
        .tree
        .tasks
        .into_iter()
        .find(|task| task.title == title)
        .unwrap()
        .id
}

/// Hosts A and B, both enrolled on the planning `plan` adds on A.
fn published_on_a(root: &Path, plan: impl FnOnce(&Host)) -> (Host, Host) {
    let a = Host::new(root, "a");
    let b = Host::new(root, "b");
    plan(&a);
    assert_eq!(a.receipt("export", &[])["route"], "bootstrap");
    let a_basis = root.join("a-basis.json");
    fs::copy(a.folder().join("project.json"), &a_basis).unwrap();
    a.commit("Publish planning from A");
    a.receipt("import", &["--basis", a_basis.to_str().unwrap()]);
    b.merge_from(&a);
    b.receipt("import", &["--empty-basis"]);
    (a, b)
}

/// A task held because this host works on it is read again at every import.
/// A works on `held`, so B's `held_change` waits on A as held. A then makes
/// its `unpublished` change and, before exporting it, takes B's Newcomer
/// under `newcomer_parent`: the import refuses and names the set-aside, which
/// A runs exactly as printed. The tasks `held_change` changed, named by their
/// titles after it, stay held for A's unpublished planning while the rest of
/// that import applies; after the export and the revert, releasing `held`
/// lets them in. Returns Newcomer and the held tasks.
fn run_printed_set_aside(
    a: &Host,
    b: &Host,
    held: engram::WorkId,
    held_change: impl FnOnce(&Host) -> Vec<&'static str>,
    unpublished: impl FnOnce(&Host),
    newcomer_parent: engram::WorkId,
) -> (engram::WorkId, Vec<engram::WorkId>) {
    a.work(&["claim", &held.0.to_string()]);
    let waiting = held_change(b);
    assert_eq!(b.receipt("export", &[])["written"], waiting.len());
    let waiting = waiting
        .into_iter()
        .map(|title| exported_id(b, title))
        .collect::<Vec<_>>();
    b.commit("Change planning under A's work on B");
    a.merge_from(b);
    assert_eq!(a.sync("import", &[]).status.code(), Some(3));

    unpublished(a);
    b.work(&["add", "Newcomer", "--under", &newcomer_parent.0.to_string()]);
    assert_eq!(b.receipt("export", &[])["written"], 1);
    let newcomer = exported_id(b, "Newcomer");
    b.commit("Add Newcomer on B");
    let before_merge = a.head();
    a.merge_from(b);
    let merged = a.head();
    let (code, message) = a.refusal("import", &[]);
    assert_eq!(code, "task_sync_divergent_planning");

    // The remedy, exactly as printed: the set-aside command the refusal
    // names, run as given, sets aside what B added since A's last import.
    let printed = message
        .split('`')
        .find(|part| part.starts_with("git restore "))
        .unwrap_or_else(|| panic!("{message}"));
    let restore = printed.split_whitespace().skip(1).collect::<Vec<_>>();
    assert!(
        restore.contains(&format!("--source={before_merge}").as_str()),
        "{printed}"
    );
    a.git(&restore);
    a.git(&["commit", "-m", "Set B's planning aside"]);
    let aside = a.head();
    let imported = a.sync("import", &[]);
    assert_eq!(
        imported.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    let imported: Value = serde_json::from_slice(&imported.stdout).unwrap();
    let still_held = imported["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|task| task["status"] == "held")
        .map(|task| {
            (
                task["task"].as_str().unwrap().to_owned(),
                task["reason"].clone(),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(
        still_held,
        waiting
            .iter()
            .map(|id| (id.0.to_string(), Value::from("local_planning_unpublished")))
            .collect::<std::collections::BTreeMap<_, _>>()
    );
    assert_eq!(a.receipt("export", &[])["written"], 1);
    a.commit("Publish A's planning");
    a.git(&["revert", "--no-edit", &aside]);
    a.work(&[
        "update",
        &held.0.to_string(),
        "--release",
        "--reason",
        "B's planning is taken",
    ]);
    assert_eq!(a.receipt("import", &[])["route"], "incremental");
    // The branch only moved forward, and both hosts' changes are here.
    a.git(&["merge-base", "--is-ancestor", &merged, "HEAD"]);
    assert_eq!(a.title(newcomer), "Newcomer");
    b.merge_from(a);
    assert_eq!(b.receipt("import", &[])["route"], "incremental");
    (newcomer, waiting)
}

#[test]
fn task_sync_cli_the_printed_set_aside_works_beside_a_held_task() {
    let fixture = test_support::temp_home().unwrap();
    let (a, b) = published_on_a(fixture.path(), |a| {
        let parent = a.add_ref("Parent");
        a.work(&["add", "Sibling", "--under", &parent]);
        a.work(&["add", "Held", "--under", &parent]);
    });
    let (parent, sibling, held) = (
        exported_id(&a, "Parent"),
        exported_id(&a, "Sibling"),
        exported_id(&a, "Held"),
    );
    run_printed_set_aside(
        &a,
        &b,
        held,
        |b| {
            b.work(&[
                "update",
                &held.0.to_string(),
                "--title",
                "Held, edited on B",
            ]);
            vec!["Held, edited on B"]
        },
        |a| {
            a.work(&[
                "update",
                &sibling.0.to_string(),
                "--title",
                "Sibling, edited on A",
            ]);
        },
        parent,
    );
    assert_eq!(a.title(held), "Held, edited on B");
    assert_eq!(a.title(sibling), "Sibling, edited on A");
    assert_eq!(b.title(sibling), "Sibling, edited on A");
}

/// A prerequisite this host dropped and has not exported still joins the
/// held task to its dependent in the graph the last import recorded.
#[test]
fn task_sync_cli_the_printed_set_aside_works_beside_a_dropped_prerequisite() {
    let fixture = test_support::temp_home().unwrap();
    let (a, b) = published_on_a(fixture.path(), |a| {
        let held = a.add_ref("Held");
        let dependent = a.add_ref("Dependent");
        a.work(&["update", &dependent, "--after", &held]);
    });
    let (held, dependent) = (exported_id(&a, "Held"), exported_id(&a, "Dependent"));
    run_printed_set_aside(
        &a,
        &b,
        held,
        |b| {
            b.work(&[
                "update",
                &held.0.to_string(),
                "--title",
                "Held, edited on B",
            ]);
            vec!["Held, edited on B"]
        },
        |a| {
            a.work(&[
                "update",
                &dependent.0.to_string(),
                "--drop-after",
                &held.0.to_string(),
            ]);
        },
        dependent,
    );
    assert_eq!(a.title(held), "Held, edited on B");
    let prerequisites = |host: &Host| {
        SqliteStore::open(project_database_path(
            &host.home,
            &ProjectId(PROJECT.into()),
        ))
        .unwrap()
        .work_prerequisites(dependent)
        .unwrap()
        .len()
    };
    assert_eq!((prerequisites(&a), prerequisites(&b)), (0, 0));
}

/// A task added under the held task waits with it, and so does one added
/// under that addition, which this host's graph would otherwise lack.
#[test]
fn task_sync_cli_the_printed_set_aside_keeps_additions_under_a_held_task_together() {
    let fixture = test_support::temp_home().unwrap();
    let (a, b) = published_on_a(fixture.path(), |a| {
        let parent = a.add_ref("Parent");
        a.work(&["add", "Sibling", "--under", &parent]);
        a.work(&["add", "Held", "--under", &parent]);
    });
    let (parent, sibling, held) = (
        exported_id(&a, "Parent"),
        exported_id(&a, "Sibling"),
        exported_id(&a, "Held"),
    );
    let (_, added) = run_printed_set_aside(
        &a,
        &b,
        held,
        |b| {
            let output = work(
                &b.home,
                &b.marker,
                &["add", "Child", "--under", &held.0.to_string(), "--json"],
            );
            let child: Value = serde_json::from_slice(&output.stdout).unwrap();
            let child = child["work"]["short_ref"].as_str().unwrap();
            b.work(&["add", "Grandchild", "--under", child]);
            vec!["Child", "Grandchild"]
        },
        |a| {
            a.work(&[
                "update",
                &sibling.0.to_string(),
                "--title",
                "Sibling, edited on A",
            ]);
        },
        parent,
    );
    let titles = added.iter().map(|id| a.title(*id)).collect::<Vec<_>>();
    assert_eq!(titles, ["Child", "Grandchild"]);
    assert_eq!(b.title(sibling), "Sibling, edited on A");
}

/// A task this host changed and has not exported is never held back with a
/// held task: when the other host links it to a held addition, the import
/// still refuses, and this host's change stays.
#[test]
fn task_sync_cli_a_link_to_a_held_addition_from_planning_changed_here_refuses() {
    let fixture = test_support::temp_home().unwrap();
    let (a, b) = published_on_a(fixture.path(), |a| {
        let parent = a.add_ref("Parent");
        a.work(&["add", "Linked", "--under", &parent]);
        a.work(&["add", "Held", "--under", &parent]);
    });
    let (linked, held) = (exported_id(&a, "Linked"), exported_id(&a, "Held"));
    a.work(&["claim", &held.0.to_string()]);
    let output = work(
        &b.home,
        &b.marker,
        &["add", "Child", "--under", &held.0.to_string(), "--json"],
    );
    let child: Value = serde_json::from_slice(&output.stdout).unwrap();
    let child = child["work"]["short_ref"].as_str().unwrap().to_owned();
    assert_eq!(b.receipt("export", &[])["written"], 1);
    b.commit("Add Child under Held on B");
    a.merge_from(&b);
    assert_eq!(a.sync("import", &[]).status.code(), Some(3));

    a.work(&[
        "update",
        &linked.0.to_string(),
        "--title",
        "Linked, renamed on A",
    ]);
    b.work(&["update", &linked.0.to_string(), "--after", &child]);
    assert_eq!(b.receipt("export", &[])["written"], 1);
    b.commit("Make Linked wait on Child on B");
    a.merge_from(&b);
    let (code, _) = a.refusal("import", &[]);
    assert_eq!(code, "task_sync_divergent_planning");
    assert_eq!(a.title(linked), "Linked, renamed on A");
}

#[test]
fn task_sync_cli_complete_import_keeps_a_retained_local_task_owed_to_the_export() {
    let fixture = test_support::temp_home().unwrap();
    let root = fixture.path();
    let a = Host::new(root, "a");
    let b = Host::new(root, "b");
    b.work(&["add", "Shared from B"]);
    assert_eq!(b.receipt("export", &[])["route"], "bootstrap");
    b.commit("Publish planning from B");
    a.work(&["add", "Only on A"]);
    assert_eq!(a.receipt("export", &[])["route"], "bootstrap");
    let a_basis = root.join("a-basis.json");
    fs::copy(a.folder().join("project.json"), &a_basis).unwrap();
    let only_on_a = directory::read_folder(&a.folder()).unwrap().tree.tasks[0].id;
    a.commit("Publish planning from A");

    // A takes B's folder in place of its own and imports it completely: B's
    // task joins, and A's own task, which no shared folder has held, stays
    // A's and is owed to A's next export.
    let source = b.checkout().to_str().unwrap().to_owned();
    a.git(&["fetch", &source, "engram-data"]);
    a.git(&["reset", "--hard", "FETCH_HEAD"]);
    let enrolled = a.receipt("import", &["--basis", a_basis.to_str().unwrap()]);
    assert_eq!(enrolled["route"], "enrol");
    assert_eq!(a.title(only_on_a), "Only on A");
    let exported = a.receipt("export", &[]);
    assert_eq!(
        (
            &exported["route"],
            &exported["written"],
            &exported["task_count"]
        ),
        (&"incremental".into(), &1.into(), &2.into())
    );
    assert_eq!(a.task_file(only_on_a)["title"], "Only on A");
}

#[test]
fn task_sync_cli_an_export_beside_a_held_task_refuses_a_graph_neither_host_wrote() {
    let fixture = test_support::temp_home().unwrap();
    let root = fixture.path();
    let a = Host::new(root, "a");
    let b = Host::new(root, "b");
    for title in ["First", "Second"] {
        a.work(&["add", title]);
    }
    assert_eq!(a.receipt("export", &[])["route"], "bootstrap");
    let a_basis = root.join("a-basis.json");
    fs::copy(a.folder().join("project.json"), &a_basis).unwrap();
    a.commit("Publish planning from A");
    let tasks = directory::read_folder(&a.folder()).unwrap().tree.tasks;
    let id = |title: &str| tasks.iter().find(|task| task.title == title).unwrap().id;
    let reference = |title: &str| id(title).0.to_string();
    // A starts on First before enrolling, so the claim is no local change.
    a.work(&["claim", &reference("First")]);
    let enrolled = a.receipt("import", &["--basis", a_basis.to_str().unwrap()]);
    assert_eq!(enrolled["route"], "enrol");
    b.merge_from(&a);
    b.receipt("import", &["--empty-basis"]);

    // B makes First wait for Second; A holds that edit while it works on First.
    b.work(&[
        "update",
        &reference("First"),
        "--after",
        &reference("Second"),
    ]);
    assert_eq!(b.receipt("export", &[])["written"], 1);
    b.commit("First waits for Second on B");
    a.merge_from(&b);
    let held = a.sync("import", &[]);
    assert_eq!(
        held.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&held.stderr)
    );

    // Here Second waits for First: valid in A's own graph, but a cycle beside
    // the held edit, which the export refuses rather than publishes.
    a.work(&[
        "update",
        &reference("Second"),
        "--after",
        &reference("First"),
    ]);
    let before = a.task_file(id("Second"));
    let (code, message) = a.refusal("export", &[]);
    assert_eq!(code, "task_sync_invalid_graph");
    assert!(message.contains("held"), "{message}");
    assert_eq!(a.task_file(id("Second")), before);
}

#[test]
fn task_sync_cli_another_hosts_exact_revert_of_a_task_exported_here_is_imported() {
    let fixture = test_support::temp_home().unwrap();
    let root = fixture.path();
    let a = Host::new(root, "a");
    let b = Host::new(root, "b");
    a.work(&["add", "Shared"]);
    assert_eq!(a.receipt("export", &[])["route"], "bootstrap");
    let a_basis = root.join("a-basis.json");
    fs::copy(a.folder().join("project.json"), &a_basis).unwrap();
    a.commit("Publish planning from A");
    assert_eq!(
        a.receipt("import", &["--basis", a_basis.to_str().unwrap()])["route"],
        "enrol"
    );
    let shared = directory::read_folder(&a.folder()).unwrap().tree.tasks[0].id;
    let reference = shared.0.to_string();
    let file = format!("tasks/{reference}.json");
    let original = fs::read(a.folder().join(&file)).unwrap();
    b.merge_from(&a);
    b.receipt("import", &["--empty-basis"]);

    // A renames the task and publishes it; B imports that, renames it back
    // and publishes the file exactly as it was at A's last import.
    a.work(&["update", &reference, "--title", "Shared, renamed on A"]);
    assert_eq!(a.receipt("export", &[])["written"], 1);
    a.commit("Rename Shared on A");
    b.merge_from(&a);
    assert_eq!(b.receipt("import", &[])["route"], "incremental");
    assert_eq!(b.title(shared), "Shared, renamed on A");
    b.work(&["update", &reference, "--title", "Shared"]);
    assert_eq!(b.receipt("export", &[])["written"], 1);
    b.commit("Rename Shared back on B");
    assert_eq!(fs::read(b.folder().join(&file)).unwrap(), original);

    // A's last import read the original file, so a diff from there shows
    // nothing for the task; the import still takes B's revert.
    a.merge_from(&b);
    assert_eq!(a.receipt("import", &[])["route"], "incremental");
    assert_eq!(a.title(shared), "Shared");
    assert_eq!(a.receipt("export", &[])["route"], "unchanged");
}

/// Two hosts enrolled on host A's published task after A renamed it,
/// published the rename and B imported it, and that task's id.
fn published_rename(root: &Path) -> (Host, Host, engram::WorkId) {
    let a = Host::new(root, "a");
    let b = Host::new(root, "b");
    a.work(&["add", "Shared"]);
    assert_eq!(a.receipt("export", &[])["route"], "bootstrap");
    let a_basis = root.join("a-basis.json");
    fs::copy(a.folder().join("project.json"), &a_basis).unwrap();
    a.commit("Publish planning from A");
    assert_eq!(
        a.receipt("import", &["--basis", a_basis.to_str().unwrap()])["route"],
        "enrol"
    );
    let id = directory::read_folder(&a.folder()).unwrap().tree.tasks[0].id;
    b.merge_from(&a);
    b.receipt("import", &["--empty-basis"]);
    let reference = id.0.to_string();
    a.work(&["update", &reference, "--title", "Renamed on A"]);
    assert_eq!(a.receipt("export", &[])["written"], 1);
    a.commit("Rename on A");
    b.merge_from(&a);
    assert_eq!(b.receipt("import", &[])["route"], "incremental");
    assert_eq!(b.title(id), "Renamed on A");
    (a, b, id)
}

/// As [`published_rename`], and A then exports and commits an unrelated new
/// task without importing, so the rename was published by an earlier export
/// than the last one.
fn published_rename_then_another_export(root: &Path) -> (Host, Host, engram::WorkId) {
    let (a, b, id) = published_rename(root);
    a.work(&["add", "Unrelated on A"]);
    assert_eq!(a.receipt("export", &[])["written"], 1);
    a.commit("Add an unrelated task on A");
    (a, b, id)
}

#[test]
fn task_sync_cli_a_local_revert_after_a_later_export_meets_another_hosts_edit_as_divergence() {
    let fixture = test_support::temp_home().unwrap();
    let (a, b, id) = published_rename_then_another_export(fixture.path());
    let reference = id.0.to_string();
    a.work(&["update", &reference, "--title", "Shared"]);
    b.work(&["update", &reference, "--title", "Renamed on A, then on B"]);
    assert_eq!(b.receipt("export", &[])["written"], 1);
    b.commit("Rename on B");
    a.merge_from(&b);
    let (code, message) = a.refusal("import", &[]);
    assert_eq!(code, "task_sync_divergent_planning");
    assert!(message.contains(&reference), "{message}");
    assert_eq!(a.title(id), "Shared");
}

#[test]
fn task_sync_cli_a_claim_after_a_later_export_takes_another_hosts_edit() {
    let fixture = test_support::temp_home().unwrap();
    let (a, b, id) = published_rename_then_another_export(fixture.path());
    let reference = id.0.to_string();
    a.work(&["claim", &reference]);
    a.work(&[
        "update",
        &reference,
        "--release",
        "--reason",
        "Leave the task to B",
    ]);
    b.work(&["update", &reference, "--title", "Renamed on A, then on B"]);
    assert_eq!(b.receipt("export", &[])["written"], 1);
    b.commit("Rename on B");
    a.merge_from(&b);
    let imported = a.receipt("import", &[]);
    assert_eq!(imported["route"], "incremental");
    assert!(
        imported["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|task| task["status"] == "applied"),
        "{imported}"
    );
    assert_eq!(a.title(id), "Renamed on A, then on B");
}

#[test]
fn task_sync_cli_a_local_revert_of_a_published_edit_meets_another_hosts_edit_as_divergence() {
    let fixture = test_support::temp_home().unwrap();
    let (a, b, id) = published_rename(fixture.path());
    let reference = id.0.to_string();
    // A returns the title to what its baseline still holds: a planning edit
    // after what A published, which another host's edit never overwrites.
    a.work(&["update", &reference, "--title", "Shared"]);
    b.work(&["update", &reference, "--title", "Renamed on A, then on B"]);
    assert_eq!(b.receipt("export", &[])["written"], 1);
    b.commit("Rename on B");
    a.merge_from(&b);
    let (code, message) = a.refusal("import", &[]);
    assert_eq!(code, "task_sync_divergent_planning");
    assert!(message.contains(&reference), "{message}");
    assert_eq!(a.title(id), "Shared");
}

#[test]
fn task_sync_cli_a_claim_on_a_published_edit_takes_another_hosts_edit() {
    let fixture = test_support::temp_home().unwrap();
    let (a, b, id) = published_rename(fixture.path());
    let reference = id.0.to_string();
    // A only claims and releases the task it published: execution, not
    // planning. A live claim would hold the incoming edit instead.
    a.work(&["claim", &reference]);
    a.work(&[
        "update",
        &reference,
        "--release",
        "--reason",
        "Leave the task to B",
    ]);
    b.work(&["update", &reference, "--title", "Renamed on A, then on B"]);
    assert_eq!(b.receipt("export", &[])["written"], 1);
    b.commit("Rename on B");
    a.merge_from(&b);
    let imported = a.receipt("import", &[]);
    assert_eq!(imported["route"], "incremental");
    assert!(
        imported["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|task| task["status"] == "applied"),
        "{imported}"
    );
    assert_eq!(a.title(id), "Renamed on A, then on B");
}

#[test]
fn task_sync_cli_an_exact_revert_of_a_rebuilt_folder_is_imported() {
    let fixture = test_support::temp_home().unwrap();
    let root = fixture.path();
    let a = Host::new(root, "a");
    let b = Host::new(root, "b");
    a.work(&["add", "Shared"]);
    assert_eq!(a.receipt("export", &[])["route"], "bootstrap");
    let a_basis = root.join("a-basis.json");
    fs::copy(a.folder().join("project.json"), &a_basis).unwrap();
    a.commit("Publish planning from A");
    assert_eq!(
        a.receipt("import", &["--basis", a_basis.to_str().unwrap()])["route"],
        "enrol"
    );
    let shared = directory::read_folder(&a.folder()).unwrap().tree.tasks[0].id;
    let reference = shared.0.to_string();
    let file = format!("tasks/{reference}.json");
    let original = fs::read(a.folder().join(&file)).unwrap();
    b.merge_from(&a);
    b.receipt("import", &["--empty-basis"]);

    // A renames the task and rewrites the complete folder; B imports that,
    // renames it back and publishes the file exactly as it was at A's last
    // import.
    a.work(&["update", &reference, "--title", "Shared, renamed on A"]);
    assert_eq!(a.receipt("export", &["--rebuild"])["route"], "rebuild");
    a.commit("Rebuild the folder on A");
    b.merge_from(&a);
    b.receipt("import", &[]);
    assert_eq!(b.title(shared), "Shared, renamed on A");
    b.work(&["update", &reference, "--title", "Shared"]);
    b.receipt("export", &[]);
    b.commit("Rename Shared back on B");
    assert_eq!(fs::read(b.folder().join(&file)).unwrap(), original);

    // No diff from A's last import shows the task, and the rebuild kept no
    // record of what it published; A imports the folder completely against
    // its own head and takes B's revert.
    a.merge_from(&b);
    assert_eq!(a.receipt("import", &[])["route"], "complete");
    assert_eq!(a.title(shared), "Shared");
    assert_eq!(a.receipt("export", &[])["route"], "unchanged");
}

#[test]
fn task_sync_cli_an_exact_revert_of_a_rebuilt_folder_is_imported_after_a_later_incremental_export()
{
    let fixture = test_support::temp_home().unwrap();
    let root = fixture.path();
    let a = Host::new(root, "a");
    let b = Host::new(root, "b");
    a.work(&["add", "Shared"]);
    a.work(&["add", "Other"]);
    assert_eq!(a.receipt("export", &[])["route"], "bootstrap");
    let a_basis = root.join("a-basis.json");
    fs::copy(a.folder().join("project.json"), &a_basis).unwrap();
    a.commit("Publish planning from A");
    assert_eq!(
        a.receipt("import", &["--basis", a_basis.to_str().unwrap()])["route"],
        "enrol"
    );
    let tasks = directory::read_folder(&a.folder()).unwrap().tree.tasks;
    let shared = tasks.iter().find(|task| task.title == "Shared").unwrap().id;
    let other = tasks.iter().find(|task| task.title == "Other").unwrap().id;
    let (shared_ref, other_ref) = (shared.0.to_string(), other.0.to_string());
    let file = format!("tasks/{shared_ref}.json");
    let original = fs::read(a.folder().join(&file)).unwrap();
    b.merge_from(&a);
    b.receipt("import", &["--empty-basis"]);

    // A renames Shared and rewrites the complete folder, then renames Other
    // and exports only that, committing both before importing.
    a.work(&["update", &shared_ref, "--title", "Shared, renamed on A"]);
    assert_eq!(a.receipt("export", &["--rebuild"])["route"], "rebuild");
    a.commit("Rebuild the folder on A");
    a.work(&["update", &other_ref, "--title", "Other, renamed on A"]);
    assert_eq!(a.receipt("export", &[])["route"], "incremental");
    a.commit("Rename Other on A");

    // B takes both, and returns Shared exactly to its bytes at A's import.
    b.merge_from(&a);
    b.receipt("import", &[]);
    b.work(&["update", &shared_ref, "--title", "Shared"]);
    b.receipt("export", &[]);
    b.commit("Rename Shared back on B");
    assert_eq!(fs::read(b.folder().join(&file)).unwrap(), original);

    // The incremental export adopts A's exact rebuild commit. Import compares
    // B's revert against that planning and takes the revert incrementally.
    a.merge_from(&b);
    assert_eq!(a.receipt("import", &[])["route"], "incremental");
    assert_eq!(a.title(shared), "Shared");
    assert_eq!(a.title(other), "Other, renamed on A");
}

#[test]
fn task_sync_cli_a_merge_keeping_the_other_hosts_version_imports_it_and_a_discarded_export_is_owed_again()
 {
    let fixture = test_support::temp_home().unwrap();
    let root = fixture.path();
    let a = Host::new(root, "a");
    let b = Host::new(root, "b");
    a.work(&["add", "Shared"]);
    assert_eq!(a.receipt("export", &[])["route"], "bootstrap");
    let a_basis = root.join("a-basis.json");
    fs::copy(a.folder().join("project.json"), &a_basis).unwrap();
    a.commit("Publish planning from A");
    let enrolled = a.receipt("import", &["--basis", a_basis.to_str().unwrap()]);
    assert_eq!(enrolled["route"], "enrol");
    let shared = directory::read_folder(&a.folder()).unwrap().tree.tasks[0].id;
    let reference = shared.0.to_string();
    b.merge_from(&a);
    b.receipt("import", &["--empty-basis"]);

    // Both hosts change the same task and commit; A merges B and keeps B's
    // version. A's own commit is still in the history, so its write was
    // published, and the import applies B's version.
    a.work(&["update", &reference, "--title", "Shared, edited on A"]);
    assert_eq!(a.receipt("export", &[])["written"], 1);
    a.commit("Edit Shared on A");
    b.work(&["update", &reference, "--title", "Shared, edited on B"]);
    assert_eq!(b.receipt("export", &[])["written"], 1);
    b.commit("Edit Shared on B");
    let source = b.checkout().to_str().unwrap().to_owned();
    a.git(&["fetch", &source, "engram-data"]);
    let data = a.data();
    let merge = Command::new("git")
        .current_dir(&data)
        .args(identity(&["merge", "--no-ff", "--no-commit", "FETCH_HEAD"]))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", data.join(".gitconfig-absent"))
        .output()
        .unwrap();
    assert!(
        merge.status.code().is_some_and(|code| code <= 1),
        "{}",
        String::from_utf8_lossy(&merge.stderr)
    );
    let path = format!(".engram/tasks/{reference}.json");
    a.git(&["checkout", "--theirs", "--", &path]);
    a.git(&["checkout", "--ours", "--", ".engram/project.json"]);
    a.git(&["add", "-A", "--", ".engram"]);
    a.git(&["commit", "-m", "Keep B's version of Shared"]);
    assert_eq!(a.receipt("import", &[])["route"], "incremental");
    assert_eq!(a.title(shared), "Shared, edited on B");

    // A change exported and then discarded, with no commit since, is owed
    // again by the next import and written by the export after it.
    a.work(&["update", &reference, "--title", "Shared, edited again on A"]);
    assert_eq!(a.receipt("export", &[])["written"], 1);
    a.git(&["checkout", "--", ".engram"]);
    assert_eq!(a.receipt("import", &[])["route"], "incremental");
    let exported = a.receipt("export", &[]);
    assert_eq!(
        (&exported["route"], &exported["written"]),
        (&"incremental".into(), &1.into())
    );
    assert_eq!(a.task_file(shared)["title"], "Shared, edited again on A");
}
