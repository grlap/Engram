//! Actual CLI selection, contention, opt-in and LF/index preservation in owned repositories.

#[path = "common/task_sync_opt_in.rs"]
mod task_sync_opt_in;
#[path = "../src/test_support.rs"]
mod test_support;

#[path = "task_sync_materialize_cli/attribution.rs"]
mod attribution;

use engram::{
    ProjectId, project_database_path,
    task_sync::{
        self,
        notes::NoteOmissions,
        progress::{ExportCapture, ExportCut, ExportProfile},
    },
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct Fixture {
    owned: test_support::TempHome,
    root: PathBuf,
    home: PathBuf,
    marker: PathBuf,
    store: PathBuf,
    setup: String,
    prepared: String,
    progress: PathBuf,
    candidate: PathBuf,
    documents: BTreeMap<String, Vec<u8>>,
}

impl Fixture {
    fn new() -> Self {
        let owned = test_support::temp_home().unwrap();
        fs::write(
            owned.path().join("global.config"),
            b"[core]\n autocrlf = true\n eol = crlf\n",
        )
        .unwrap();
        let root = owned.path().join("repository");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "--initial-branch=ordinary"]);
        git(&root, &["config", "core.autocrlf", "false"]);
        for (key, name) in [
            ("core.attributesFile", "attributes"),
            ("core.excludesFile", "excludes"),
        ] {
            let path = owned.path().join(name);
            fs::write(&path, b"").unwrap();
            git(&root, &["config", key, path.to_str().unwrap()]);
        }
        let marker = root.join(".engram-project");
        fs::write(&marker, b"materialize-cli\n").unwrap();
        fs::write(root.join(".gitignore"), b"/.engram/\n").unwrap();
        fs::write(root.join(".gitattributes"), b"*.md text eol=lf\n").unwrap();
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
        let home = owned.path().join("home");
        let project = ProjectId("materialize-cli".into());
        task_sync_opt_in::enable(&home, &project.0);
        let store = project_database_path(&home, &project);
        fs::write(&store, b"source must not be opened as SQLite").unwrap();
        let progress = owned.path().join("progress");
        let candidate = owned.path().join("candidate");
        let tree = task_sync::TaskTree {
            project: task_sync::ProjectDocument {
                format: task_sync::FORMAT.into(),
                project_id: project.clone(),
                notes_included: false,
            },
            tasks: vec![task_sync::TaskDocument {
                id: engram::WorkId::new(),
                external_ref: None,
                title: "Shared task".into(),
                outcome: "Deliver it".into(),
                acceptance: vec!["Implemented".into()],
                acceptance_bindings: Vec::new(),
                evaluation_mode: None,
                kind: engram::WorkItemKind::Task,
                priority: 2,
                labels: Vec::new(),
                assigned_to: None,
                deferred_until: None,
                lifecycle: engram::WorkLifecycle::Open,
                parent_id: None,
                child_requirement: engram::ChildRequirement::Required,
                prerequisites: Vec::new(),
                blockers: Vec::new(),
                superseded_by: None,
                disposal_reason: None,
                completion: None,
                notes: Vec::new(),
                activity: BTreeMap::new(),
            }],
        };
        let documents = tree.documents().unwrap();
        let prepared = task_sync::progress::prepare(
            &candidate,
            &progress,
            &store,
            &project,
            &ExportProfile {
                include_notes: false,
                origin_machine: None,
            },
            false,
            |_| {
                Ok(ExportCapture {
                    tree,
                    omissions: NoteOmissions::default(),
                    activity_omissions: NoteOmissions::default(),
                    cut: ExportCut {
                        position: 0,
                        anchor: None,
                    },
                    full_reason: None,
                })
            },
        )
        .unwrap()
        .operation
        .to_string();
        let mut fixture = Self {
            owned,
            root,
            home,
            marker,
            store,
            setup: String::new(),
            prepared,
            progress,
            candidate,
            documents,
        };
        let output = fixture
            .command()
            .args(["setup-prepare"])
            .arg(&fixture.root)
            .args([
                "--branch",
                "ordinary",
                "--visibility",
                "private",
                "--readership",
                "fixture maintainers",
            ])
            .output()
            .unwrap();
        let setup: Value = success(&output);
        fixture.setup = setup["operation"].as_str().unwrap().into();
        fixture
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_engram"));
        isolate(&mut command, &self.root);
        command
            .current_dir(&self.root)
            .arg("--home")
            .arg(&self.home)
            .arg("--project-file")
            .arg(&self.marker)
            .args([
                "task-sync",
                "--actor-id",
                "fixture-operator",
                "--session-id",
                "fixture-cli",
            ]);
        command
    }

    fn materialize(&self) -> Output {
        self.materialize_command().output().unwrap()
    }

    fn materialize_command(&self) -> Command {
        let mut command = self.command();
        command
            .arg("setup-materialize")
            .arg(&self.root)
            .arg("--setup-operation")
            .arg(&self.setup)
            .arg("--prepared-operation")
            .arg(&self.prepared)
            .arg("--progress-directory")
            .arg(&self.progress)
            .arg("--candidate-directory")
            .arg(&self.candidate);
        command
    }
}

fn isolate(command: &mut Command, root: &Path) {
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
        .env("HOME", root.parent().unwrap())
        .env("XDG_CONFIG_HOME", root.parent().unwrap())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env(
            "GIT_CONFIG_GLOBAL",
            root.parent().unwrap().join("global.config"),
        )
        .env_remove("ENGRAM_ACTOR_CONTEXT");
}

fn git(root: &Path, args: &[&str]) -> Vec<u8> {
    let mut command = Command::new("git");
    isolate(&mut command, root);
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
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn success(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut result = BTreeMap::new();
    for entry in fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            result.insert(path.clone(), None);
            result.extend(snapshot(&path));
        } else {
            result.insert(path.clone(), Some(fs::read(path).unwrap()));
        }
    }
    result
}

#[test]
fn task_materialize_cli_keeps_lf_bytes_and_index_unchanged_before_operator_staging() {
    let fixture = Fixture::new();
    // Local autocrlf and the isolated global eol would normalize without the block.
    git(&fixture.root, &["config", "core.autocrlf", "true"]);
    let index = fs::read(fixture.root.join(".git/index")).unwrap();
    let receipt = success(&fixture.materialize());
    let completion: Value = serde_json::from_slice(
        &fs::read(fixture.root.join(format!(
            ".git/engram-task-sync-materialize-{}-{}/complete",
            fixture.setup, fixture.prepared
        )))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(completion["operation"], receipt["operation"]);
    assert_eq!(receipt["repeated"], false);
    assert!(receipt["redactor"].as_str().unwrap().contains("no-op"));
    assert_eq!(fs::read(fixture.root.join(".git/index")).unwrap(), index);
    assert_eq!(
        fs::read(&fixture.store).unwrap(),
        b"source must not be opened as SQLite"
    );
    for (path, bytes) in &fixture.documents {
        assert_eq!(
            &fs::read(fixture.root.join(format!(".engram/{path}"))).unwrap(),
            bytes
        );
    }
    let before = snapshot(fixture.owned.path());
    assert_eq!(
        success(&fixture.materialize())["operation"],
        receipt["operation"]
    );
    assert_eq!(snapshot(fixture.owned.path()), before);
    // This is the fixture operator's staging, after proving the command did none.
    git(
        &fixture.root,
        &["add", ".engram/project.json", ".engram/tasks"],
    );
    for (path, bytes) in &fixture.documents {
        assert_eq!(
            &git(&fixture.root, &["show", &format!(":.engram/{path}")]),
            bytes
        );
    }
}

#[test]
fn task_materialize_cli_off_and_contended_existing_lock_create_no_state() {
    let fixture = Fixture::new();
    success(
        &fixture
            .command()
            .args(["disable", "--json"])
            .output()
            .unwrap(),
    );
    let before = snapshot(fixture.owned.path());
    let output = fixture.materialize();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("not enabled"));
    assert_eq!(snapshot(fixture.owned.path()), before);
    let fixture = Fixture::new();
    let before = snapshot(fixture.owned.path());
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.root.join(".git/engram-task-sync-setup.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    let output = fixture.materialize();
    lock.unlock().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("busy"));
    assert_eq!(snapshot(fixture.owned.path()), before);
}

#[test]
fn task_materialize_cli_refuses_parent_components_before_private_state() {
    let fixture = Fixture::new();
    for selected in 0..3 {
        let paths = [&fixture.root, &fixture.progress, &fixture.candidate];
        let invalid = Path::new("unused/../selection");
        let before = snapshot(fixture.owned.path());
        let output = fixture
            .command()
            .arg("setup-materialize")
            .arg(if selected == 0 { invalid } else { paths[0] })
            .arg("--setup-operation")
            .arg(&fixture.setup)
            .arg("--prepared-operation")
            .arg(&fixture.prepared)
            .arg("--progress-directory")
            .arg(if selected == 1 { invalid } else { paths[1] })
            .arg("--candidate-directory")
            .arg(if selected == 2 { invalid } else { paths[2] })
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("selection paths must not contain '..'")
        );
        assert_eq!(snapshot(fixture.owned.path()), before);
    }
}

#[test]
fn task_sync_cli_start_guards_preserve_all_state_when_setting_is_not_enabled() {
    for state in [
        "absent",
        "disabled",
        "malformed",
        "oversized",
        "nonordinary",
    ] {
        let fixture = Fixture::new();
        let setting = fixture
            .store
            .parent()
            .unwrap()
            .join("task-sync-setting.json");
        match state {
            "absent" => fs::remove_file(&setting).unwrap(),
            "disabled" => {
                success(
                    &fixture
                        .command()
                        .args(["disable", "--json"])
                        .output()
                        .unwrap(),
                );
            }
            "malformed" => fs::write(&setting, b"invalid JSON").unwrap(),
            "oversized" => fs::write(&setting, vec![b' '; 128 * 1024]).unwrap(),
            "nonordinary" => {
                fs::rename(&setting, fixture.owned.path().join("original-setting")).unwrap();
                fs::create_dir(&setting).unwrap();
            }
            _ => unreachable!(),
        }
        let before = snapshot(fixture.owned.path());
        let export = fixture
            .command()
            .arg("export")
            .arg(fixture.owned.path().join("new-export"))
            .output()
            .unwrap();
        assert_eq!(export.status.code(), Some(1), "{state}");
        for dry in [false, true] {
            let mut command = fixture.command();
            command.arg("import").arg(&fixture.candidate);
            if dry {
                command.arg("--dry-run");
            }
            assert_eq!(command.output().unwrap().status.code(), Some(1), "{state}");
        }
        let prepare = fixture
            .command()
            .arg("prepare")
            .arg(fixture.owned.path().join("new-prepare"))
            .arg("--progress-directory")
            .arg(fixture.owned.path().join("new-progress"))
            .output()
            .unwrap();
        assert_eq!(prepare.status.code(), Some(1), "{state}");
        let setup = fixture
            .command()
            .arg("setup-prepare")
            .arg(&fixture.root)
            .args([
                "--branch",
                "ordinary",
                "--visibility",
                "private",
                "--readership",
                "fixture maintainers",
            ])
            .output()
            .unwrap();
        assert_eq!(setup.status.code(), Some(1), "{state}");
        assert_eq!(fixture.materialize().status.code(), Some(1), "{state}");
        assert_eq!(snapshot(fixture.owned.path()), before, "{state}");
    }
}
