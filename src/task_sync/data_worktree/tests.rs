use super::*;
use std::fs;

mod advance;
mod commit;
mod evidence;
mod mixed_planning;
mod reserved_path;

#[test]
fn administration_queries_keep_the_original_operation_deadline() {
    let started = Instant::now();
    let budget = Budget::within(started, Duration::from_secs(60)).unwrap();
    super::budget_probe::install(Some((0, Duration::from_secs(30))));
    let remaining = budget.bound().unwrap();
    let (bounds, stalled) = super::budget_probe::finish();
    assert!(stalled && bounds == [remaining]);
    assert!(remaining <= Duration::from_secs(30));
    assert!(!remaining.is_zero());

    super::budget_probe::install(Some((0, Duration::from_secs(60))));
    let error = budget.bound().unwrap_err();
    let (bounds, stalled) = super::budget_probe::finish();
    assert!(stalled && bounds.is_empty());
    assert!(matches!(
        error.downcast_ref::<git::QueryFailure>(),
        Some(git::QueryFailure::Deadline)
    ));
}

struct Fixture {
    home: crate::test_support::TempHome,
    code: PathBuf,
    data: PathBuf,
}

fn run(root: &Path, args: &[&str]) -> Vec<u8> {
    let mut command = git::command(root, args);
    command
        .env("GIT_AUTHOR_NAME", "Data fixture")
        .env("GIT_AUTHOR_EMAIL", "data@example.invalid")
        .env("GIT_COMMITTER_NAME", "Data fixture")
        .env("GIT_COMMITTER_EMAIL", "data@example.invalid")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", root.join("absent-global-config"));
    git::run_query_command(
        command,
        args[0],
        false,
        None,
        Duration::from_secs(30),
        git::QueryStderrPolicy::ErrorOnly,
    )
    .unwrap()
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self::code_only(true);
        run(
            &fixture.code,
            &[
                "worktree",
                "add",
                "--detach",
                "--no-checkout",
                fixture.data.to_str().unwrap(),
                "HEAD",
            ],
        );
        run(
            &fixture.data,
            &["symbolic-ref", "HEAD", "refs/heads/engram-data"],
        );
        run(&fixture.data, &["read-tree", "--empty"]);
        fixture
    }

    fn code_only(ignored: bool) -> Self {
        let home = crate::test_support::temp_home().unwrap();
        let code = home.path().join("code");
        fs::create_dir(&code).unwrap();
        run(&code, &["init", "--initial-branch=code"]);
        fs::write(
            code.join(".gitignore"),
            if ignored {
                "/target/\n/.engram-data/\n"
            } else {
                "/target/\n"
            },
        )
        .unwrap();
        fs::write(code.join(".engram-project"), "data/fixture\n").unwrap();
        fs::write(code.join("code.rs"), "fn main() {}\n").unwrap();
        run(&code, &["add", ".gitignore", "code.rs", ".engram-project"]);
        run(&code, &["commit", "-m", "Seed code"]);
        fs::create_dir(code.join("target")).unwrap();
        let data = code.join(DIRECTORY_NAME);
        Self { home, code, data }
    }

    fn code_snapshot(&self) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        (
            run(&self.code, &["rev-parse", "HEAD"]),
            fs::read(self.code.join(".git/index")).unwrap(),
            run(
                &self.code,
                &["status", "--porcelain=v1", "--untracked-files=all"],
            ),
        )
    }
}

#[test]
fn orphan_data_export_and_commit_leave_code_head_index_and_worktree_unchanged() {
    let fixture = Fixture::new();
    let before = fixture.code_snapshot();
    let selected = DataWorktree::observe(&fixture.code, &fixture.data, "engram-data").unwrap();
    assert_eq!(selected.head, None);
    assert_ne!(selected.git_directory, selected.common_directory);
    fs::create_dir(fixture.data.join(".engram")).unwrap();
    fs::write(fixture.data.join(".engram/project.json"), "{}\n").unwrap();
    selected.check_data_paths().unwrap();
    run(&fixture.data, &["add", ".engram/project.json"]);
    run(&fixture.data, &["commit", "-m", "Export metadata"]);
    let committed = DataWorktree::observe(&fixture.code, &fixture.data, "engram-data").unwrap();
    assert!(committed.head.is_some());
    assert_eq!(run(&fixture.data, &["rev-list", "--count", "HEAD"]), b"1\n");
    assert_eq!(fixture.code_snapshot(), before);
    assert!(!fixture.code.join(".engram").exists());
}

#[test]
fn data_worktree_refuses_foreign_index_untracked_and_ignored_files() {
    let fixture = Fixture::new();
    let selected = DataWorktree::observe(&fixture.code, &fixture.data, "engram-data").unwrap();
    fs::write(fixture.data.join("foreign.rs"), "preserve me\n").unwrap();
    assert!(selected.check_data_paths().is_err());
    run(&fixture.data, &["add", "foreign.rs"]);
    fs::remove_file(fixture.data.join("foreign.rs")).unwrap();
    assert!(selected.check_data_paths().is_err());
    run(&fixture.data, &["rm", "--cached", "foreign.rs"]);
    fs::write(
        selected.common_directory.join("info/exclude"),
        "secret.rs\n",
    )
    .unwrap();
    fs::write(
        fixture.data.join("secret.rs"),
        "ignored but not shared data\n",
    )
    .unwrap();
    assert!(selected.check_data_paths().is_err());
    assert_eq!(
        fs::read_to_string(fixture.data.join("secret.rs")).unwrap(),
        "ignored but not shared data\n"
    );
}

#[test]
fn incoming_commit_admission_checks_the_selected_tree_not_only_current_head() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.data.join(".engram")).unwrap();
    fs::write(fixture.data.join(".engram/project.json"), "{}\n").unwrap();
    run(&fixture.data, &["add", ".engram/project.json"]);
    run(&fixture.data, &["commit", "-m", "Initial task data"]);
    let selected = DataWorktree::observe(&fixture.code, &fixture.data, "engram-data").unwrap();
    let before = fixture.code_snapshot();
    selected
        .check_commit(selected.head.as_ref().unwrap())
        .unwrap();
    let code_head = String::from_utf8(run(&fixture.code, &["rev-parse", "HEAD"])).unwrap();
    assert!(
        selected
            .check_commit(code_head.trim())
            .unwrap_err()
            .to_string()
            .contains("orphan history")
    );
    // This exact object is not HEAD. A caller must not smuggle an otherwise
    // retained code file through a task-only snapshot of an incoming commit.
    fs::write(fixture.data.join("foreign.rs"), "preserve me\n").unwrap();
    run(&fixture.data, &["add", "foreign.rs"]);
    let tree = String::from_utf8(run(&fixture.data, &["write-tree"])).unwrap();
    let incoming = String::from_utf8(run(
        &fixture.data,
        &["commit-tree", tree.trim(), "-m", "Foreign payload"],
    ))
    .unwrap();
    assert!(
        selected
            .check_commit(incoming.trim())
            .unwrap_err()
            .to_string()
            .contains("outside the .engram/")
    );
    assert_eq!(
        selected.head.as_ref().unwrap(),
        String::from_utf8(run(&fixture.data, &["rev-parse", "HEAD"]))
            .unwrap()
            .trim()
    );
    assert_eq!(fixture.code_snapshot(), before);
}

#[test]
fn admitted_folder_reads_policy_files_without_exporting_them_as_task_documents() {
    let fixture = Fixture::new();
    let tree = crate::task_sync::TaskTree {
        project: crate::task_sync::ProjectDocument {
            format: crate::task_sync::FORMAT.into(),
            project_id: crate::ProjectId("data/fixture".into()),
            notes_included: false,
        },
        tasks: Vec::new(),
    };
    let profile = crate::task_sync::progress::ExportProfile {
        include_notes: false,
        origin_machine: None,
    };
    let path = fixture.data.join(".engram");
    crate::task_sync::directory::write_folder(&path, &tree, &profile).unwrap();
    assert_eq!(
        crate::task_sync::directory::read_resolved_folder(&path)
            .unwrap()
            .tree,
        tree
    );
    fs::write(path.join(".gitignore"), "# operator policy\n").unwrap();
    fs::write(path.join(".gitattributes"), "* text eol=lf -filter\n").unwrap();
    let selected = DataWorktree::observe(&fixture.code, &fixture.data, "engram-data").unwrap();
    let before = fixture.code_snapshot();
    let read = crate::task_sync::directory::read_resolved_data_folder(&selected).unwrap();
    assert_eq!(read.tree, tree);
    assert_eq!(read.profile, profile);
    assert!(
        crate::task_sync::directory::read_resolved_folder(&path).is_err(),
        "standalone folder candidates must retain their strict layout"
    );
    fs::write(path.join("unexpected.json"), "{}\n").unwrap();
    assert!(crate::task_sync::directory::read_resolved_data_folder(&selected).is_err());
    assert_eq!(fixture.code_snapshot(), before);
}

#[test]
fn code_history_and_unignored_worktrees_are_not_adopted_as_data() {
    let fixture = Fixture::new();
    run(&fixture.data, &["switch", "-c", "borrowed", "code"]);
    assert!(
        DataWorktree::observe(&fixture.code, &fixture.data, "borrowed")
            .unwrap_err()
            .to_string()
            .contains("orphan history")
    );
    let visible = fixture.code.join("visible-data");
    run(
        &fixture.code,
        &[
            "worktree",
            "add",
            "--detach",
            "--no-checkout",
            visible.to_str().unwrap(),
            "HEAD",
        ],
    );
    run(
        &visible,
        &["symbolic-ref", "HEAD", "refs/heads/visible-data"],
    );
    run(&visible, &["read-tree", "--empty"]);
    assert!(
        DataWorktree::observe(&fixture.code, &visible, "visible-data")
            .unwrap_err()
            .to_string()
            .contains("fixed .engram-data directory")
    );
    fs::write(fixture.code.join(".gitignore"), "/target/\n").unwrap();
    assert!(
        DataWorktree::observe(&fixture.code, &fixture.data, "borrowed")
            .unwrap_err()
            .to_string()
            .contains("ignored by the code checkout")
    );
    assert!(DataWorktree::observe(&fixture.code, &fixture.code, "code").is_err());
    assert!(DataWorktree::observe(&fixture.code, &fixture.data, "other-data").is_err());
}

#[test]
fn initialization_creates_locked_orphan_data_and_preserves_code_source_identity() {
    let fixture = Fixture::code_only(false);
    let before = fixture.code_snapshot();
    let source = crate::source::inspect(&fixture.code, Duration::from_secs(30)).source_id;
    assert!(
        source.is_some(),
        "code source must be observable before initialization"
    );
    let before_ignore = fs::read(fixture.code.join(".gitignore")).unwrap();
    let info = fixture.code.join(".git/info/exclude");
    fs::write(&info, b"# retain unterminated operator rule").unwrap();
    let receipt = initialize(
        &fixture.code.join(".engram-project"),
        &crate::ProjectId("data/fixture".into()),
        "engram-data",
    )
    .unwrap();
    assert!(receipt.created && receipt.locked);
    assert_eq!(receipt.checkout, fs::canonicalize(&fixture.data).unwrap());
    let selected = DataWorktree::observe(&fixture.code, &fixture.data, "engram-data").unwrap();
    assert_eq!(selected.head, None);
    assert!(selected.git_directory.join("locked").is_file());
    assert_eq!(
        fs::read(&info).unwrap(),
        b"# retain unterminated operator rule\n/.engram-data/\n"
    );
    assert_eq!(
        fs::read(fixture.code.join(".gitignore")).unwrap(),
        before_ignore
    );
    assert_eq!(fixture.code_snapshot(), before);
    assert!(
        crate::source::inspect(&fixture.code, Duration::from_secs(30)).source_id == source,
        "data initialization changed code source identity"
    );
    let local_rules = fs::read(&info).unwrap();
    let repeated = initialize(
        &fixture.code.join(".engram-project"),
        &crate::ProjectId("data/fixture".into()),
        "engram-data",
    )
    .unwrap();
    assert!(!repeated.created && repeated.locked);
    assert_eq!(fs::read(&info).unwrap(), local_rules);
    run(
        &fixture.code,
        &["worktree", "unlock", fixture.data.to_str().unwrap()],
    );
    assert!(!selected.git_directory.join("locked").exists());
    let relocked = initialize(
        &fs::canonicalize(fixture.code.join(".engram-project")).unwrap(),
        &crate::ProjectId("data/fixture".into()),
        "engram-data",
    )
    .unwrap();
    assert!(!relocked.created && relocked.locked);
    assert_eq!(relocked.checkout, receipt.checkout);
    assert!(selected.git_directory.join("locked").is_file());
    assert_eq!(fixture.code_snapshot(), before);
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.code.join(".git/engram-data.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    let error = initialize(
        &fixture.code.join(".engram-project"),
        &crate::ProjectId("data/fixture".into()),
        "engram-data",
    )
    .unwrap_err();
    assert!(error.to_string().contains("initialization busy"));
    lock.unlock().unwrap();
    assert!(
        query(
            &fixture.code,
            &["worktree", "remove", fixture.data.to_str().unwrap()],
            false
        )
        .is_err()
    );
    assert!(fixture.data.is_dir());
}

#[test]
fn initialization_preserves_occupied_paths_branches_and_missing_locked_registration() {
    let fixture = Fixture::code_only(false);
    let info = fixture.code.join(".git/info/exclude");
    let rules = fs::read(&info).unwrap();
    let marker = fixture.code.join(".engram-project");
    let project = crate::ProjectId("data/fixture".into());
    assert!(initialize(&marker, &project, "code").is_err());
    assert_eq!(fs::read(&info).unwrap(), rules);
    fs::create_dir(&fixture.data).unwrap();
    fs::write(fixture.data.join("sentinel"), b"unowned data").unwrap();
    assert!(initialize(&marker, &project, "engram-data").is_err());
    assert_eq!(
        fs::read(fixture.data.join("sentinel")).unwrap(),
        b"unowned data"
    );
    assert_eq!(fs::read(&info).unwrap(), rules);

    let fixture = Fixture::code_only(false);
    let marker = fixture.code.join(".engram-project");
    initialize(&marker, &project, "engram-data").unwrap();
    let selected = DataWorktree::observe(&fixture.code, &fixture.data, "engram-data").unwrap();
    let preserved = fixture.home.path().join("temporarily-absent-data");
    fs::rename(&fixture.data, &preserved).unwrap();
    run(&fixture.code, &["worktree", "prune", "--expire=now"]);
    assert!(selected.git_directory.join("locked").is_file());
    assert!(initialize(&marker, &project, "engram-data").is_err());
    assert!(preserved.join(".git").is_file());
    assert!(!fixture.data.exists());
}

#[test]
fn initialization_refuses_a_linked_exclusion_file_without_touching_its_target() {
    let fixture = Fixture::code_only(false);
    let target = fixture.home.path().join("operator-rules");
    fs::write(&target, b"preserve rules\n").unwrap();
    let exclude = fixture.code.join(".git/info/exclude");
    fs::rename(&exclude, fixture.home.path().join("original-exclude")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &exclude).unwrap();
    #[cfg(windows)]
    {
        // Ordinary files also exercise refusal before any append when the
        // bounded input is not a regular file; no symlink privilege is needed.
        fs::create_dir(&exclude).unwrap();
    }
    assert!(
        initialize(
            &fixture.code.join(".engram-project"),
            &crate::ProjectId("data/fixture".into()),
            "engram-data"
        )
        .is_err()
    );
    assert_eq!(fs::read(&target).unwrap(), b"preserve rules\n");
    assert!(!fixture.data.exists());
}
