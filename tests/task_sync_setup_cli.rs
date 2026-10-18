//! Real local Git fixtures; setup preparation never opens the selected store.

#[path = "../src/test_support.rs"]
mod test_support;

#[path = "common/task_sync_opt_in.rs"]
mod task_sync_opt_in;

#[path = "task_sync_setup_cli/attribution.rs"]
mod attribution;

use serde_json::Value;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct Fixture {
    owned: test_support::TempHome,
    root: PathBuf,
    admin: PathBuf,
    home: PathBuf,
    global_config: PathBuf,
}

fn isolate_git(command: &mut Command, global: &Path) {
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_CONFIG",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
        "GIT_ATTR_SOURCE",
    ] {
        command.env_remove(name);
    }
    command
        // Keep Git's ordinary global config locations inside this fixture too.
        .env("HOME", global.parent().unwrap())
        .env("XDG_CONFIG_HOME", global.parent().unwrap())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", global)
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
}

fn git(root: &Path, args: &[&str]) -> Vec<u8> {
    let mut command = Command::new("git");
    isolate_git(&mut command, &root.parent().unwrap().join(".gitconfig"));
    let output = command
        .current_dir(root)
        .args([
            "--no-optional-locks",
            "-c",
            "core.hooksPath=",
            "-c",
            "core.fsmonitor=",
        ])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

impl Fixture {
    fn new(separate: bool) -> Self {
        let owned = test_support::temp_home().unwrap();
        let global_config = owned.path().join(".gitconfig");
        fs::write(&global_config, "[core]\n attributesFile =\n").unwrap();
        let root = owned.path().join("repository");
        fs::create_dir(&root).unwrap();
        let admin = if separate {
            owned.path().join("administration")
        } else {
            root.join(".git")
        };
        let mut args = vec!["init", "--initial-branch=ordinary"];
        let separate_arg = format!("--separate-git-dir={}", admin.display());
        if separate {
            args.push(&separate_arg);
        }
        git(&root, &args);
        fs::write(root.join(".engram-project"), "fixture-project\n").unwrap();
        fs::write(
            root.join(".gitignore"),
            b"# private state\r\n/.engram/\r\n/target/\r\n*.db\r\n",
        )
        .unwrap();
        fs::write(root.join(".gitattributes"), b"*.md text eol=lf\n").unwrap();
        fs::write(root.join("unrelated.txt"), "ordinary code\n").unwrap();
        git(&root, &["add", "."]);
        git(
            &root,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-m",
                "fixture",
            ],
        );
        let home = owned.path().join("store-home");
        task_sync_opt_in::enable(&home, "fixture-project");
        Self {
            owned,
            root,
            admin,
            home,
            global_config,
        }
    }

    fn command(&self, root: &Path, marker: &Path) -> Command {
        self.command_with_readership(root, marker, "project maintainers")
    }

    fn command_with_readership(&self, root: &Path, marker: &Path, readership: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_engram"));
        // Setup is synchronous; avoid a CPU-sized idle pool per concurrent child.
        command.env("TOKIO_WORKER_THREADS", "1");
        isolate_git(&mut command, &self.global_config);
        command
            .env_remove("ENGRAM_ACTOR_CONTEXT")
            .current_dir(&self.root)
            .arg("--home")
            .arg(&self.home)
            .arg("--project-file")
            .arg(marker)
            .args([
                "task-sync",
                "--actor-id",
                "fixture-operator",
                "--session-id",
                "fixture-session",
                "setup-prepare",
            ])
            .arg(root)
            .args([
                "--branch",
                "ordinary",
                "--visibility",
                "private",
                "--readership",
                readership,
            ]);
        command
    }

    fn prepare(&self) -> Output {
        self.command(&self.root, &self.root.join(".engram-project"))
            .output()
            .unwrap()
    }

    fn intent_path(&self) -> PathBuf {
        self.admin.join("engram-task-sync-setup.json")
    }

    fn intent(&self) -> Value {
        serde_json::from_slice(&fs::read(self.intent_path()).unwrap()).unwrap()
    }

    fn success(&self) -> Value {
        let output = self.prepare();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn refusal(&self, expected: &str) {
        let output = self.prepare();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains(expected), "{error}");
        assert!(!self.intent_path().exists());
        assert!(!self.admin.join("engram-task-sync-setup.lock").exists());
        task_sync_opt_in::assert_no_database(&self.home);
    }
}

fn bytes(value: &Value) -> Vec<u8> {
    serde_json::from_value(value.clone()).unwrap()
}

fn commit_fixture(fixture: &Fixture) {
    git(
        &fixture.root,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-m",
            "fixture basis",
        ],
    );
}

fn assert_refused(output: &Output, expected: &str) {
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "unexpected success");
    assert!(error.contains(expected), "{error}");
}

#[test]
fn setup_refuses_ignore_directory_before_a_child_filter_can_execute() {
    let fixture = Fixture::new(false);
    git(&fixture.root, &["rm", ".gitignore"]);
    fs::create_dir(fixture.root.join(".gitignore")).unwrap();
    let payload = ".gitignore/payload";
    fs::write(fixture.root.join(payload), "tracked original\n").unwrap();
    git(&fixture.root, &["add", payload]);
    commit_fixture(&fixture);
    fs::write(
        fixture.admin.join("info/attributes"),
        ".gitignore/payload filter=sentinel\n",
    )
    .unwrap();
    git(
        &fixture.root,
        &[
            "config",
            "filter.sentinel.clean",
            "echo invoked > ignore-filter-sentinel.txt; cat",
        ],
    );
    fs::write(fixture.root.join(payload), "modified child\n").unwrap();
    let index = fs::read(fixture.admin.join("index")).unwrap();
    fixture.refusal("setup input must be an ordinary bounded file");
    assert!(!fixture.root.join("ignore-filter-sentinel.txt").exists());
    assert_eq!(fs::read(fixture.admin.join("index")).unwrap(), index);
    assert_eq!(
        fs::read(fixture.root.join(payload)).unwrap(),
        b"modified child\n"
    );
}

#[test]
fn setup_refuses_occupied_filtered_task_before_git_can_execute_its_filter() {
    let fixture = Fixture::new(false);
    let task = fixture.root.join(".engram/tasks/specific.json");
    fs::create_dir_all(task.parent().unwrap()).unwrap();
    fs::write(&task, "original\n").unwrap();
    git(&fixture.root, &["add", "-f", ".engram/tasks/specific.json"]);
    commit_fixture(&fixture);
    fs::write(
        fixture.admin.join("info/attributes"),
        ".engram/tasks/specific.json filter=sentinel\n",
    )
    .unwrap();
    git(
        &fixture.root,
        &[
            "config",
            "filter.sentinel.clean",
            "echo invoked > filter-sentinel.txt; cat",
        ],
    );
    fs::write(&task, "modified and must not be filtered\n").unwrap();
    let index = fs::read(fixture.admin.join("index")).unwrap();
    fixture.refusal("occupied");
    assert!(!fixture.root.join("filter-sentinel.txt").exists());
    assert_eq!(fs::read(fixture.admin.join("index")).unwrap(), index);
    assert_eq!(
        fs::read(task).unwrap(),
        b"modified and must not be filtered\n"
    );
}

#[test]
fn setup_checks_selected_custom_marker_with_literal_pathspecs() {
    for state in [
        "clean",
        "dirty",
        "staged",
        "untracked",
        "filtered",
        "conflicted",
    ] {
        let fixture = Fixture::new(false);
        let name = "custom[marker]";
        let marker = fixture.root.join(name);
        fs::rename(fixture.root.join(".engram-project"), &marker).unwrap();
        git(&fixture.root, &["add", "-A"]);
        commit_fixture(&fixture);
        match state {
            "clean" => (),
            "dirty" | "staged" => {
                fs::write(&marker, "fixture-project\n\n").unwrap();
                if state == "staged" {
                    git(&fixture.root, &["--literal-pathspecs", "add", name]);
                }
            }
            "untracked" => {
                git(
                    &fixture.root,
                    &["--literal-pathspecs", "rm", "--cached", name],
                );
                commit_fixture(&fixture);
            }
            "filtered" => {
                // Git attribute patterns are not pathspecs: escape brackets here.
                fs::write(
                    fixture.admin.join("info/attributes"),
                    "custom\\[marker\\] filter=sentinel\n",
                )
                .unwrap();
                git(
                    &fixture.root,
                    &[
                        "config",
                        "filter.sentinel.clean",
                        "echo invoked > filter-sentinel.txt; cat",
                    ],
                );
                fs::write(&marker, "fixture-project\n\n").unwrap();
            }
            "conflicted" => {
                let blob =
                    String::from_utf8(git(&fixture.root, &["rev-parse", "HEAD:custom[marker]"]))
                        .unwrap();
                let mut command = Command::new("git");
                isolate_git(&mut command, &fixture.global_config);
                let mut child = command
                    .current_dir(&fixture.root)
                    .args(["update-index", "--index-info"])
                    .stdin(std::process::Stdio::piped())
                    .spawn()
                    .unwrap();
                writeln!(
                    child.stdin.take().unwrap(),
                    "0 {}\t{name}\n100644 {} 1\t{name}\n100644 {} 2\t{name}",
                    "0".repeat(blob.trim().len()),
                    blob.trim(),
                    blob.trim()
                )
                .unwrap();
                assert!(child.wait().unwrap().success());
            }
            _ => unreachable!(),
        }
        let index = fs::read(fixture.admin.join("index")).unwrap();
        let output = fixture.command(&fixture.root, &marker).output().unwrap();
        if state == "clean" {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        } else {
            assert_refused(
                &output,
                if state == "filtered" {
                    "filter=sentinel"
                } else {
                    "dirty, staged or conflicted"
                },
            );
            assert!(!fixture.intent_path().exists());
            assert!(!fixture.admin.join("engram-task-sync-setup.lock").exists());
        }
        assert!(!fixture.root.join("filter-sentinel.txt").exists());
        assert_eq!(fs::read(fixture.admin.join("index")).unwrap(), index);
        task_sync_opt_in::assert_no_database(&fixture.home);
    }
}

#[test]
fn setup_refuses_suppressed_setup_inspection_and_absent_reserved_index_entries() {
    for flag in ["--assume-unchanged", "--skip-worktree"] {
        for name in [".gitignore", ".gitattributes", ".engram-project"] {
            let fixture = Fixture::new(false);
            git(&fixture.root, &["update-index", flag, name]);
            fs::write(
                fixture.root.join(name),
                if name == ".engram-project" {
                    "fixture-project\n\n"
                } else {
                    "# dirty bytes hidden from status\n"
                },
            )
            .unwrap();
            let index = fs::read(fixture.admin.join("index")).unwrap();
            fixture.refusal("index inspection is suppressed");
            assert_eq!(fs::read(fixture.admin.join("index")).unwrap(), index);
        }
        for name in [".engram/project.json", ".engram/tasks/specific.json"] {
            let fixture = Fixture::new(false);
            let path = fixture.root.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "unowned shared state\n").unwrap();
            git(&fixture.root, &["add", "-f", name]);
            commit_fixture(&fixture);
            git(&fixture.root, &["update-index", flag, name]);
            fs::remove_file(&path).unwrap();
            if name.contains("tasks/") {
                fs::remove_dir(path.parent().unwrap()).unwrap();
            }
            let index = fs::read(fixture.admin.join("index")).unwrap();
            fixture.refusal("occupied in index");
            assert_eq!(fs::read(fixture.admin.join("index")).unwrap(), index);
        }
    }
    let fixture = Fixture::new(false);
    fs::create_dir(fixture.root.join(".engram")).unwrap();
    fs::write(fixture.root.join(".engram/project.json"), "unowned").unwrap();
    git(&fixture.root, &["add", "-f", ".engram/project.json"]);
    commit_fixture(&fixture);
    git(&fixture.root, &["rm", ".engram/project.json"]);
    fixture.refusal("occupied in HEAD");
}

#[test]
fn setup_refuses_a_non_directory_engram_on_disk_in_the_index_and_in_head() {
    // A regular file on disk hides the reserved paths below it.
    let file = Fixture::new(false);
    fs::write(file.root.join(".engram"), "not a directory\n").unwrap();
    let index = fs::read(file.admin.join("index")).unwrap();
    file.refusal("reserved shared path is occupied: .engram is not a directory");
    assert_eq!(
        fs::read(file.root.join(".engram")).unwrap(),
        b"not a directory\n"
    );
    assert_eq!(fs::read(file.admin.join("index")).unwrap(), index);

    // An indexed file whose working copy is gone.
    let indexed = Fixture::new(false);
    fs::write(indexed.root.join(".engram"), "indexed\n").unwrap();
    git(&indexed.root, &["add", "-f", ".engram"]);
    fs::remove_file(indexed.root.join(".engram")).unwrap();
    let index = fs::read(indexed.admin.join("index")).unwrap();
    indexed.refusal("reserved shared path is occupied in index: .engram");
    assert!(!indexed.root.join(".engram").exists());
    assert_eq!(fs::read(indexed.admin.join("index")).unwrap(), index);

    // A file only HEAD still holds.
    let retained = Fixture::new(false);
    fs::write(retained.root.join(".engram"), "committed\n").unwrap();
    git(&retained.root, &["add", "-f", ".engram"]);
    commit_fixture(&retained);
    git(&retained.root, &["rm", "-q", ".engram"]);
    let index = fs::read(retained.admin.join("index")).unwrap();
    retained.refusal("reserved shared path is occupied in HEAD: .engram");
    assert!(!retained.root.join(".engram").exists());
    assert_eq!(fs::read(retained.admin.join("index")).unwrap(), index);
}

#[test]
fn setup_ignore_proposals_reopen_only_root_shared_paths_across_ignore_sources() {
    for (source, rule) in [
        ("root", "/.engram/"),
        ("root", ".engram/"),
        ("root", ".engram"),
        ("root", "**/.engram"),
        ("info", ".engram/"),
        ("global", ".engram/"),
    ] {
        let fixture = Fixture::new(false);
        match source {
            "root" => {
                fs::write(fixture.root.join(".gitignore"), format!("{rule}\n")).unwrap();
                git(&fixture.root, &["add", ".gitignore"]);
                commit_fixture(&fixture);
            }
            "info" => {
                fs::write(fixture.root.join(".gitignore"), "# unrelated\n").unwrap();
                git(&fixture.root, &["add", ".gitignore"]);
                commit_fixture(&fixture);
                fs::write(fixture.admin.join("info/exclude"), rule).unwrap();
            }
            "global" => {
                fs::write(fixture.root.join(".gitignore"), "# unrelated\n").unwrap();
                git(&fixture.root, &["add", ".gitignore"]);
                commit_fixture(&fixture);
                let excludes = fixture.owned.path().join("global.exclude");
                fs::write(&excludes, rule).unwrap();
                fs::write(
                    &fixture.global_config,
                    format!(
                        "[core]\n attributesFile =\n excludesFile = \"{}\"\n",
                        excludes.display().to_string().replace('\\', "/")
                    ),
                )
                .unwrap();
                let actual = git(&fixture.root, &["config", "--get", "core.excludesFile"]);
                assert_eq!(
                    String::from_utf8(actual).unwrap().trim(),
                    excludes.display().to_string().replace('\\', "/")
                );
            }
            _ => unreachable!(),
        }
        fs::create_dir(fixture.root.join(".engram")).unwrap();
        fs::write(fixture.root.join(".engram/private.db"), "private").unwrap();
        assert_eq!(
            git(&fixture.root, &["check-ignore", ".engram/private.db"]),
            b".engram/private.db\n"
        );
        fixture.success();
        let proposed = bytes(&fixture.intent()["basis"]["edits"][0]["after"]);
        // Apply only within this disposable fixture to assess Git's policy,
        // independently of preparation (which must leave the checkout alone).
        fs::write(fixture.root.join(".gitignore"), proposed).unwrap();
        fs::create_dir_all(fixture.root.join(".engram/tasks")).unwrap();
        for (name, ignored) in [
            (".engram/project.json", false),
            (
                ".engram/tasks/00000000-0000-7000-8000-000000000001.json",
                false,
            ),
            (".engram/private.db", true),
            (".engram/tasks/private.db", true),
            (".engram/tasks/deeper/private.json", true),
        ] {
            let path = fixture.root.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "fixture bytes").unwrap();
            let mut command = Command::new("git");
            isolate_git(&mut command, &fixture.global_config);
            let output = command
                .current_dir(&fixture.root)
                .args(["check-ignore", "--no-index", "--quiet", "--", name])
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(i32::from(!ignored)),
                "{source} {rule} {name}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    let nested = Fixture::new(false);
    fs::create_dir(nested.root.join(".engram")).unwrap();
    fs::write(nested.root.join(".engram/.gitignore"), "*.json\n").unwrap();
    nested.refusal(".engram/.gitignore");
}

#[test]
fn setup_default_process_attribution_can_repeat_and_retains_first_provenance() {
    let fixture = Fixture::new(false);
    let run = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_engram"));
        command.env("TOKIO_WORKER_THREADS", "1");
        isolate_git(&mut command, &fixture.global_config);
        for name in [
            "ENGRAM_ACTOR_ID",
            "ENGRAM_SESSION_ID",
            "ENGRAM_ACTOR_CONTEXT",
            "USERNAME",
            "USER",
            "LOGNAME",
        ] {
            command.env_remove(name);
        }
        let output = command
            .current_dir(&fixture.root)
            .arg("--home")
            .arg(&fixture.home)
            .arg("--project-file")
            .arg(fixture.root.join(".engram-project"))
            .args([
                "task-sync",
                "--actor-context",
                "fixture-context",
                "--source-skill",
                "fixture-skill",
                "setup-prepare",
            ])
            .arg(&fixture.root)
            .args([
                "--branch",
                "ordinary",
                "--visibility",
                "private",
                "--readership",
                "maintainers",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let first = run();
    let original = fs::read(fixture.intent_path()).unwrap();
    let second = run();
    assert_eq!(second["operation"], first["operation"]);
    assert_eq!(second["repeated"], true);
    assert_eq!(fs::read(fixture.intent_path()).unwrap(), original);
    let attribution = &fixture.intent()["attribution"];
    assert!(
        attribution["actor_id"]
            .as_str()
            .unwrap()
            .starts_with("local-user-")
    );
    assert!(
        attribution["session_id"]
            .as_str()
            .unwrap()
            .starts_with("local-process-v1-")
    );
    assert_eq!(attribution["actor_context"], "fixture-context");
    assert_eq!(attribution["source_skill"], "fixture-skill");
    task_sync_opt_in::assert_no_database(&fixture.home);
}

#[test]
fn setup_maximal_admitted_inputs_produce_a_readable_exact_repeat() {
    let fixture = Fixture::new(false);
    let mut comment = vec![b'~'; 64 * 1024];
    comment[0] = b'#';
    comment[64 * 1024 - 1] = b'\n';
    for name in [".gitignore", ".gitattributes"] {
        fs::write(fixture.root.join(name), &comment).unwrap();
    }
    let mut marker = b"fixture-project".to_vec();
    marker.resize(4096, b' ');
    fs::write(fixture.root.join(".engram-project"), &marker).unwrap();
    git(&fixture.root, &["add", "."]);
    commit_fixture(&fixture);
    let first = fixture.success();
    let original = fs::read(fixture.intent_path()).unwrap();
    assert!(original.len() <= 2 * 1024 * 1024);
    let second = fixture.success();
    assert_eq!(second["operation"], first["operation"]);
    assert_eq!(second["repeated"], true);
    assert_eq!(fs::read(fixture.intent_path()).unwrap(), original);
    let intent = fixture.intent();
    assert_eq!(bytes(&intent["basis"]["marker"]), marker);
    assert_eq!(bytes(&intent["basis"]["edits"][0]["before"]), comment);
}

#[test]
fn setup_first_call_race_publishes_complete_lock_and_preserves_unowned_empty_lock() {
    let fixture = Fixture::new(false);
    let children: Vec<_> = (0..4)
        .map(|_| {
            fixture
                .command(&fixture.root, &fixture.root.join(".engram-project"))
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in children {
        let output = child.wait_with_output().unwrap();
        if !output.status.success() {
            assert_refused(&output, "busy");
        }
    }
    assert_eq!(
        fs::read(fixture.admin.join("engram-task-sync-setup.lock")).unwrap(),
        b"engram-task-sync-setup-lock-v1\n"
    );
    fixture.success();
    let unowned = Fixture::new(false);
    let lock = unowned.admin.join("engram-task-sync-setup.lock");
    fs::write(&lock, []).unwrap();
    assert_refused(&unowned.prepare(), "unowned setup lock path");
    assert_eq!(fs::read(lock).unwrap(), b"");
    assert!(!unowned.intent_path().exists());
}

#[test]
fn setup_prepares_private_raw_edits_and_exact_repeat_without_mutating_local_state() {
    let fixture = Fixture::new(false);
    let index = fs::read(fixture.admin.join("index")).unwrap();
    let config = fs::read(fixture.admin.join("config")).unwrap();
    let head = fs::read(fixture.admin.join("HEAD")).unwrap();
    let ignore = fs::read(fixture.root.join(".gitignore")).unwrap();
    let attributes = fs::read(fixture.root.join(".gitattributes")).unwrap();
    fs::create_dir(fixture.root.join(".engram")).unwrap();
    fs::write(fixture.root.join(".engram/private.db"), "private bytes").unwrap();
    fs::write(fixture.root.join("unrelated.txt"), "local edit to preserve").unwrap();
    let first = fixture.success();
    assert_eq!(first["prepared"], true);
    assert_eq!(first["enabled"], false);
    assert_eq!(first["repeated"], false);
    assert!(
        first["assurance"]
            .as_str()
            .unwrap()
            .contains("not authenticated")
    );
    assert!(first["redactor"].as_str().unwrap().contains("no-op"));
    let original_intent = fs::read(fixture.intent_path()).unwrap();
    let second = fixture.success();
    assert_eq!(second["operation"], first["operation"]);
    assert_eq!(second["repeated"], true);
    assert_eq!(fs::read(fixture.intent_path()).unwrap(), original_intent);
    let intent = fixture.intent();
    assert_eq!(intent["basis"]["project_id"], "fixture-project");
    assert_eq!(intent["basis"]["branch"], "ordinary");
    assert_eq!(
        intent["basis"]["assertions"]["readership"],
        "project maintainers"
    );
    assert_eq!(bytes(&intent["basis"]["edits"][0]["before"]), ignore);
    assert_eq!(bytes(&intent["basis"]["edits"][1]["before"]), attributes);
    assert_eq!(bytes(&intent["basis"]["edits"][0]["after"]), b"# private state\r\n/target/\r\n*.db\r\n# BEGIN ENGRAM TASK SYNC\n!/.engram/\n/.engram/*\n!/.engram/project.json\n!/.engram/tasks/\n/.engram/tasks/*\n!/.engram/tasks/*.json\n# END ENGRAM TASK SYNC\n");
    assert_eq!(bytes(&intent["basis"]["edits"][1]["after"]), b"*.md text eol=lf\n# BEGIN ENGRAM TASK SYNC\n/.engram/** text eol=lf -filter diff merge -working-tree-encoding -ident\n# END ENGRAM TASK SYNC\n");
    for (path, original) in [
        (fixture.admin.join("index"), index),
        (fixture.admin.join("config"), config),
        (fixture.admin.join("HEAD"), head),
        (fixture.root.join(".gitignore"), ignore),
        (fixture.root.join(".gitattributes"), attributes),
    ] {
        assert_eq!(fs::read(path).unwrap(), original);
    }
    assert_eq!(
        fs::read(fixture.root.join(".engram/private.db")).unwrap(),
        b"private bytes"
    );
    assert!(!fixture.root.join(".engram/project.json").exists());
    assert!(!fixture.root.join(".engram/tasks").exists());
    task_sync_opt_in::assert_no_database(&fixture.home);
}

#[test]
fn setup_supports_actual_separate_git_directory_and_alias_without_notes() {
    let fixture = Fixture::new(true);
    let output = fixture
        .command(&fixture.root, &fixture.root.join(".engram-project"))
        .current_dir(fixture.owned.path())
        .args(["--origin-machine", "workstation-1"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let intent = fixture.intent();
    assert_eq!(
        intent["basis"]["assertions"]["origin_machine"],
        "workstation-1"
    );
    assert_eq!(intent["basis"]["assertions"]["include_notes"], false);
    assert!(fixture.root.join(".git").is_file());
    task_sync_opt_in::assert_no_database(&fixture.home);
}

#[test]
fn setup_resolves_relative_git_directory_from_outside_repository() {
    let fixture = Fixture::new(false);
    let top_level =
        String::from_utf8(git(&fixture.root, &["rev-parse", "--show-toplevel"])).unwrap();
    let top_level = Path::new(top_level.strip_suffix('\n').unwrap());
    assert!(top_level.is_absolute());
    assert_eq!(
        fs::canonicalize(top_level).unwrap(),
        fs::canonicalize(&fixture.root).unwrap()
    );
    let output = fixture
        .command(&fixture.root, &fixture.root.join(".engram-project"))
        .current_dir(fixture.owned.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "relative .git path from an outside cwd: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let intent = fixture.intent();
    assert_eq!(intent["basis"]["branch"], "ordinary");
    assert!(fixture.admin.join("config").is_file());
    task_sync_opt_in::assert_no_database(&fixture.home);
}

#[test]
fn setup_admits_configured_global_filter_that_does_not_apply() {
    let fixture = Fixture::new(false);
    let global = b"[core]\n attributesFile =\n[filter \"lfs\"]\n clean = fixture-filter-must-not-run\n process = fixture-process-must-not-run\n required = true\n";
    fs::write(&fixture.global_config, global).unwrap();
    fixture.success();
    assert_eq!(fs::read(&fixture.global_config).unwrap(), global);
}

#[test]
fn setup_refuses_applied_attributes_by_path_before_status_or_private_writes() {
    for attribute in [
        "filter=lfs",
        "diff=custom",
        "merge=custom",
        "merge=text",
        "working-tree-encoding=UTF-16",
        "eol=crlf",
        "ident",
    ] {
        let fixture = Fixture::new(false);
        fs::write(
            fixture.root.join(".gitattributes"),
            format!(".engram/** {attribute}\n"),
        )
        .unwrap();
        git(&fixture.root, &["add", ".gitattributes"]);
        git(
            &fixture.root,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-m",
                "attribute basis",
            ],
        );
        let before = fs::read(fixture.root.join(".gitattributes")).unwrap();
        fixture.refusal(attribute.split('=').next().unwrap());
        assert!(
            String::from_utf8_lossy(&fixture.prepare().stderr).contains(".engram/project.json")
        );
        assert_eq!(
            fs::read(fixture.root.join(".gitattributes")).unwrap(),
            before
        );
    }
    let setup = Fixture::new(false);
    // info/attributes has the highest precedence and is queried as-is.
    fs::write(
        setup.admin.join("info/attributes"),
        ".gitignore filter=lfs\n",
    )
    .unwrap();
    git(
        &setup.root,
        &[
            "config",
            "filter.lfs.process",
            "fixture-process-must-not-run",
        ],
    );
    git(&setup.root, &["config", "filter.lfs.required", "true"]);
    setup.refusal("filter=lfs applies to .gitignore");
}

#[test]
fn setup_refuses_nested_attributes_and_admits_current_lf_policy() {
    let nested = Fixture::new(false);
    fs::create_dir(nested.root.join(".engram")).unwrap();
    fs::write(
        nested.root.join(".engram/.gitattributes"),
        "*.json filter=lfs\n",
    )
    .unwrap();
    nested.refusal("nested attribute file is occupied");
    let lf = Fixture::new(false);
    fs::write(
        lf.admin.join("info/attributes"),
        ".engram/** text eol=lf -filter diff merge -working-tree-encoding -ident\n",
    )
    .unwrap();
    lf.success();
}

#[test]
#[cfg(windows)]
fn setup_refuses_case_aliases_in_reserved_head_index_and_setup_names() {
    let removed = Fixture::new(false);
    fs::create_dir(removed.root.join(".ENGRAM")).unwrap();
    fs::write(removed.root.join(".ENGRAM/project.json"), "unowned\n").unwrap();
    git(&removed.root, &["add", "-f", ".ENGRAM/project.json"]);
    commit_fixture(&removed);
    git(&removed.root, &["rm", ".ENGRAM/project.json"]);
    let index = fs::read(removed.admin.join("index")).unwrap();
    removed.refusal("reserved shared path is occupied in HEAD");
    assert_eq!(fs::read(removed.admin.join("index")).unwrap(), index);

    for flag in ["--skip-worktree", "--assume-unchanged"] {
        let absent = Fixture::new(false);
        fs::create_dir_all(absent.root.join(".ENGRAM/tasks")).unwrap();
        let task = ".ENGRAM/tasks/item.json";
        fs::write(absent.root.join(task), "unowned\n").unwrap();
        git(&absent.root, &["add", "-f", task]);
        commit_fixture(&absent);
        git(&absent.root, &["update-index", flag, task]);
        fs::remove_file(absent.root.join(task)).unwrap();
        fs::remove_dir(absent.root.join(".ENGRAM/tasks")).unwrap();
        fs::remove_dir(absent.root.join(".ENGRAM")).unwrap();
        let index = fs::read(absent.admin.join("index")).unwrap();
        absent.refusal("reserved shared path is occupied in index");
        assert_eq!(fs::read(absent.admin.join("index")).unwrap(), index);
    }

    for original in [".gitignore", ".gitattributes", ".engram-project"] {
        let alias = Fixture::new(false);
        let upper = original.to_ascii_uppercase();
        git(&alias.root, &["mv", original, "rename-intermediate"]);
        git(&alias.root, &["mv", "rename-intermediate", &upper]);
        commit_fixture(&alias);
        let index = fs::read(alias.admin.join("index")).unwrap();
        // The canonical marker name may itself preserve its on-disk case.
        // Root setup aliases must still be refused before status.
        if original == ".engram-project" {
            git(&alias.root, &["update-index", "--assume-unchanged", &upper]);
            let index = fs::read(alias.admin.join("index")).unwrap();
            alias.refusal("index inspection is suppressed");
            assert_eq!(fs::read(alias.admin.join("index")).unwrap(), index);
        } else {
            alias.refusal("case alias of setup path in index");
            assert_eq!(fs::read(alias.admin.join("index")).unwrap(), index);
        }
    }
}

#[test]
fn setup_proposed_attributes_preserve_text_diffs_and_merge_conflict_markers() {
    for global in [false, true] {
        let fixture = Fixture::new(false);
        for (key, value) in [
            (
                "merge.text.driver",
                "echo invoked > named-text-sentinel.txt; exit 0",
            ),
            ("merge.default", "sentinel-default"),
            (
                "merge.sentinel-default.driver",
                "echo invoked > default-merge-sentinel.txt; exit 0",
            ),
        ] {
            let mut args = vec!["config"];
            if global {
                args.extend(["--file", fixture.global_config.to_str().unwrap()]);
            }
            args.extend([key, value]);
            git(&fixture.root, &args);
        }
        assert_eq!(
            git(&fixture.root, &["config", "--get", "merge.default"]),
            b"sentinel-default\n"
        );
        assert_eq!(
            git(&fixture.root, &["config", "--get", "merge.text.driver"]),
            b"echo invoked > named-text-sentinel.txt; exit 0\n"
        );
        fixture.success();
        let intent = fixture.intent();
        // Apply only in this owned disposable fixture to verify actual Git semantics.
        fs::write(
            fixture.root.join(".gitattributes"),
            bytes(&intent["basis"]["edits"][1]["after"]),
        )
        .unwrap();
        let task = ".engram/tasks/item.json";
        fs::create_dir_all(fixture.root.join(".engram/tasks")).unwrap();
        fs::write(fixture.root.join(task), "{\n  \"title\": \"base\"\n}\n").unwrap();
        git(&fixture.root, &["add", ".gitattributes"]);
        git(&fixture.root, &["add", "-f", task]);
        commit_fixture(&fixture);
        git(&fixture.root, &["switch", "-c", "other"]);
        fs::write(fixture.root.join(task), "{\n  \"title\": \"other\"\n}\n").unwrap();
        let diff = String::from_utf8(git(&fixture.root, &["diff", "--", task])).unwrap();
        assert!(diff.contains("-  \"title\": \"base\""), "{diff}");
        assert!(diff.contains("+  \"title\": \"other\""), "{diff}");
        assert!(!diff.contains("Binary files"), "{diff}");
        git(&fixture.root, &["add", "-f", task]);
        commit_fixture(&fixture);
        git(&fixture.root, &["switch", "ordinary"]);
        fs::write(fixture.root.join(task), "{\n  \"title\": \"ours\"\n}\n").unwrap();
        git(&fixture.root, &["add", "-f", task]);
        commit_fixture(&fixture);
        let mut command = Command::new("git");
        isolate_git(&mut command, &fixture.global_config);
        let output = command
            .current_dir(&fixture.root)
            .args([
                "-c",
                "core.hooksPath=",
                "-c",
                "core.fsmonitor=",
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "merge",
                "--no-commit",
                "other",
            ])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let conflict = fs::read_to_string(fixture.root.join(task)).unwrap();
        assert!(conflict.contains("<<<<<<< HEAD\n"), "{conflict}");
        assert!(conflict.contains("=======\n"), "{conflict}");
        assert!(conflict.contains(">>>>>>> other\n"), "{conflict}");
        assert!(conflict.contains("\"ours\""), "{conflict}");
        assert!(conflict.contains("\"other\""), "{conflict}");
        assert!(!fixture.root.join("named-text-sentinel.txt").exists());
        assert!(!fixture.root.join("default-merge-sentinel.txt").exists());
        task_sync_opt_in::assert_no_database(&fixture.home);
    }
}

#[test]
fn setup_refuses_sparse_detached_and_wrong_branches_before_any_private_write() {
    let sparse = Fixture::new(false);
    git(&sparse.root, &["config", "core.sparseCheckout", "true"]);
    sparse.refusal("sparse checkout");
    let detached = Fixture::new(false);
    git(&detached.root, &["checkout", "--detach"]);
    detached.refusal("symbolic-ref");
    let wrong = Fixture::new(false);
    git(&wrong.root, &["checkout", "-b", "different"]);
    wrong.refusal("attached branch differs");
}

#[test]
fn setup_refuses_linked_pointer_bare_and_ambiguous_roots() {
    let fixture = Fixture::new(false);
    let linked = fixture.owned.path().join("linked");
    git(
        &fixture.root,
        &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
    );
    let output = fixture
        .command(&linked, &linked.join(".engram-project"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("linked worktree"));
    assert!(!fixture.intent_path().exists());
    let bare = fixture.owned.path().join("bare");
    git(&fixture.root, &["init", "--bare", bare.to_str().unwrap()]);
    fs::write(bare.join(".engram-project"), "fixture-project").unwrap();
    let output = fixture
        .command(&bare, &bare.join(".engram-project"))
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&output.stderr).contains("bare repository"));
    let child = fixture.root.join("child");
    fs::create_dir(&child).unwrap();
    fs::write(child.join(".engram-project"), "fixture-project").unwrap();
    let output = fixture
        .command(&child, &child.join(".engram-project"))
        .output()
        .unwrap();
    // The hardened query cannot climb past the selected root's parent.
    assert!(String::from_utf8_lossy(&output.stderr).contains("local Git admission query refused"));
    let output = fixture
        .command(&fixture.root, &child.join(".engram-project"))
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&output.stderr).contains("marker must belong"));
    assert!(!fixture.intent_path().exists());
}

#[cfg(windows)]
#[test]
fn setup_windows_case_aliases_bind_same_repository_and_exact_intent() {
    let fixture = Fixture::new(false);
    let first = fixture.success();
    let alias = PathBuf::from(fixture.root.to_str().unwrap().to_uppercase());
    let output = fixture
        .command(&alias, &alias.join(".engram-project"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let repeat: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(repeat["operation"], first["operation"]);
    assert_eq!(repeat["repeated"], true);
}

#[test]
fn setup_refuses_dirty_staged_conflicted_setup_and_ignored_reserved_occupancy() {
    for name in [".gitignore", ".gitattributes", ".engram-project"] {
        for staged in [false, true] {
            let fixture = Fixture::new(false);
            fs::write(
                fixture.root.join(name),
                if name == ".engram-project" {
                    "fixture-project\n\n"
                } else {
                    "changed\n"
                },
            )
            .unwrap();
            if staged {
                git(&fixture.root, &["add", name]);
            }
            fixture.refusal("dirty, staged or conflicted");
        }
    }
    for name in ["project.json", "tasks"] {
        let fixture = Fixture::new(false);
        fs::create_dir(fixture.root.join(".engram")).unwrap();
        let occupied = fixture.root.join(".engram").join(name);
        if name == "tasks" {
            fs::create_dir(&occupied).unwrap();
        } else {
            fs::write(&occupied, "equal bytes are still unowned").unwrap();
        }
        fixture.refusal("occupied");
        assert!(occupied.exists());
    }
    let fixture = Fixture::new(false);
    let blob = String::from_utf8(git(&fixture.root, &["rev-parse", "HEAD:.gitignore"])).unwrap();
    let mut command = Command::new("git");
    isolate_git(&mut command, &fixture.global_config);
    let mut child = command
        .current_dir(&fixture.root)
        .args(["update-index", "--index-info"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(
        child.stdin.take().unwrap(),
        "0 {}\t.gitignore\n100644 {} 1\t.gitignore\n100644 {} 2\t.gitignore",
        "0".repeat(blob.trim().len()),
        blob.trim(),
        blob.trim()
    )
    .unwrap();
    assert!(child.wait().unwrap().success());
    fixture.refusal("dirty, staged or conflicted");
}

#[test]
fn setup_refuses_unknown_scope_invalid_alias_and_notes_without_alias() {
    let fixture = Fixture::new(false);
    let output = fixture
        .command_with_readership(&fixture.root, &fixture.root.join(".engram-project"), " ")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("readership must be nonblank"));
    for args in [
        vec!["--origin-machine", "bad/alias"],
        vec!["--include-notes"],
    ] {
        let output = fixture
            .command(&fixture.root, &fixture.root.join(".engram-project"))
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!fixture.intent_path().exists());
        assert!(!fixture.admin.join("engram-task-sync-setup.lock").exists());
    }
    let output = fixture
        .command(&fixture.root, &fixture.root.join(".engram-project"))
        .args(["--include-notes", "--origin-machine", "local"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fixture.intent()["basis"]["assertions"]["include_notes"],
        true
    );
}

#[test]
fn setup_preserves_divergent_and_unowned_private_records_and_refuses_busy_lock() {
    let fixture = Fixture::new(false);
    fixture.success();
    let before = fs::read(fixture.intent_path()).unwrap();
    let output = fixture
        .command(&fixture.root, &fixture.root.join(".engram-project"))
        .args(["--origin-machine", "different"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(fs::read(fixture.intent_path()).unwrap(), before);
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.admin.join("engram-task-sync-setup.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    let output = fixture.prepare();
    assert!(String::from_utf8_lossy(&output.stderr).contains("busy"));
    assert_eq!(fs::read(fixture.intent_path()).unwrap(), before);
    drop(lock);
    let unowned = Fixture::new(false);
    fs::write(unowned.intent_path(), "unowned bytes").unwrap();
    assert!(!unowned.prepare().status.success());
    assert_eq!(fs::read(unowned.intent_path()).unwrap(), b"unowned bytes");
    let unowned_lock = Fixture::new(false);
    let lock_path = unowned_lock.admin.join("engram-task-sync-setup.lock");
    fs::write(&lock_path, "not an owned lock").unwrap();
    let output = unowned_lock.prepare();
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{error}");
    assert!(error.contains("unowned setup lock path"), "{error}");
    assert_eq!(fs::read(&lock_path).unwrap(), b"not an owned lock");
    assert!(!unowned_lock.intent_path().exists());
}

#[test]
fn setup_refuses_an_intent_whose_head_an_unrelated_commit_moved_and_keeps_its_bytes() {
    let fixture = Fixture::new(false);
    let head = |fixture: &Fixture| {
        String::from_utf8(git(&fixture.root, &["rev-parse", "HEAD"]))
            .unwrap()
            .trim()
            .to_owned()
    };
    let prepared_head = head(&fixture);
    fixture.success();
    assert_eq!(fixture.intent()["basis"]["head"], prepared_head.as_str());
    let intent = fs::read(fixture.intent_path()).unwrap();
    let lock_path = fixture.admin.join("engram-task-sync-setup.lock");
    let lock = fs::read(&lock_path).unwrap();
    // The commit touches no setup or reserved path, so HEAD is the only part
    // of the recorded basis it changes.
    fs::write(
        fixture.root.join("unrelated.txt"),
        "ordinary code, revised\n",
    )
    .unwrap();
    git(&fixture.root, &["add", "unrelated.txt"]);
    commit_fixture(&fixture);
    assert_ne!(head(&fixture), prepared_head);
    let output = fixture.prepare();
    assert_refused(
        &output,
        "existing setup intent has a different basis or assertion",
    );
    assert_eq!(fs::read(fixture.intent_path()).unwrap(), intent);
    assert_eq!(fs::read(&lock_path).unwrap(), lock);
    let pending: Vec<_> = fs::read_dir(&fixture.admin)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("engram-task-sync-setup-") && name.ends_with(".pending"))
        .collect();
    assert!(pending.is_empty(), "{pending:?}");
    task_sync_opt_in::assert_no_database(&fixture.home);
}

#[test]
fn setup_proposals_handle_missing_files_and_preserve_unterminated_unrelated_bytes() {
    let fixture = Fixture::new(false);
    fs::remove_file(fixture.root.join(".gitignore")).unwrap();
    fs::write(fixture.root.join(".gitattributes"), b"*.md text eol=lf").unwrap();
    git(&fixture.root, &["add", "."]);
    git(
        &fixture.root,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-m",
            "setup basis",
        ],
    );
    fixture.success();
    let intent = fixture.intent();
    assert!(intent["basis"]["edits"][0]["before"].is_null());
    assert!(bytes(&intent["basis"]["edits"][1]["after"]).starts_with(b"*.md text eol=lf\n# BEGIN"));
    assert!(!fixture.root.join(".gitignore").exists());
    assert_eq!(
        fs::read(fixture.root.join(".gitattributes")).unwrap(),
        b"*.md text eol=lf"
    );
}

#[cfg(windows)]
#[test]
fn setup_rejects_junction_ancestors_without_touching_their_targets() {
    let fixture = Fixture::new(false);
    let outside = fixture.owned.path().join("private-target");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("sentinel"), "private").unwrap();
    let link = fixture.root.join(".engram");
    junction::create(&outside, &link).unwrap();
    let output = fixture.prepare();
    // Unlink before assertions so an assertion failure cannot strand a junction.
    junction::delete(link).unwrap();
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{error}");
    assert!(error.contains("linked path refused"), "{error}");
    assert!(!fixture.intent_path().exists());
    assert!(!fixture.admin.join("engram-task-sync-setup.lock").exists());
    task_sync_opt_in::assert_no_database(&fixture.home);
    assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"private");
}
