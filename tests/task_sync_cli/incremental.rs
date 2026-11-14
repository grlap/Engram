//! The ordinary route between two hosts with their own checkouts: export,
//! commit, merge, commit, import, writing and reading only changed tasks.

use super::*;

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

    fn receipt(&self, operation: &str, extra: &[&str]) -> Value {
        let output = self.sync(operation, extra);
        assert!(
            output.status.success(),
            "{operation} {extra:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

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
