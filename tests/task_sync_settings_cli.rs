//! Settings operate without SQLite and never infer opt-in from repository files.

#[path = "../src/test_support.rs"]
mod test_support;

use engram::{ProjectId, project_database_path};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, process::Command};

struct Fixture {
    owned: test_support::TempHome,
    root: PathBuf,
    home: PathBuf,
    database: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let owned = test_support::temp_home().unwrap();
        let root = owned.path().join("repository");
        fs::create_dir(&root).unwrap();
        fs::write(root.join(".engram-project"), "fixture/task-sync\n").unwrap();
        let home = owned.path().join("engram-home");
        let database = project_database_path(&home, &ProjectId("fixture/task-sync".into()));
        Self {
            owned,
            root,
            home,
            database,
        }
    }

    fn command(&self, action: &str) -> Command {
        self.command_as(action, "fixture-human")
    }

    fn command_as(&self, action: &str, actor: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_engram"));
        command
            .current_dir(&self.root)
            .env_remove("ENGRAM_ACTOR_ID")
            .env_remove("ENGRAM_SESSION_ID")
            .arg("--home")
            .arg(&self.home)
            .arg("--project-file")
            .arg(self.root.join(".engram-project"))
            .args([
                "task-sync",
                "--actor-id",
                actor,
                "--session-id",
                "fixture-session",
                action,
                "--json",
            ]);
        command
    }

    fn call(&self, action: &str, args: &[&str]) -> Value {
        let output = self.command(action).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn refusal(&self, action: &str, args: &[&str], code: &str) -> Value {
        let output = self.command(action).args(args).output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["format"], "engram-task-sync-error-v1");
        assert_eq!(result["operation"], action);
        assert_eq!(result["error"]["code"], code);
        result
    }

    fn setting(&self) -> PathBuf {
        self.database
            .parent()
            .unwrap()
            .join("task-sync-setting.json")
    }

    fn enable(&self) -> Value {
        self.call(
            "enable",
            &[
                "--user-instruction",
                "Enable Git task sync for this project.",
            ],
        )
    }
}

#[test]
fn settings_absence_is_disabled_without_creating_home_or_database() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.root.join(".engram")).unwrap();
    fs::write(
        fixture.root.join(".engram/project.json"),
        b"unowned repository data",
    )
    .unwrap();
    let status = fixture.call("status", &[]);
    assert_eq!(status["format"], "engram-task-sync-status-v1");
    assert_eq!(status["project_id"], "fixture/task-sync");
    assert_eq!(status["state"], "disabled");
    assert_eq!(status["reason"], "not configured (off by default)");
    assert!(status["user_instruction"].is_null());
    assert!(
        status["effect"]
            .as_str()
            .unwrap()
            .contains("on this machine")
    );
    assert!(!fixture.home.exists());
    assert_eq!(
        fs::read(fixture.root.join(".engram/project.json")).unwrap(),
        b"unowned repository data"
    );
}

#[test]
fn settings_cli_refuses_misplaced_instruction_flags_without_changing_settings() {
    for existing in [false, true] {
        let fixture = Fixture::new();
        let original = existing.then(|| {
            fixture.enable();
            fs::read(fixture.setting()).unwrap()
        });
        let entries = existing.then(|| {
            let mut names = fs::read_dir(fixture.database.parent().unwrap())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>();
            names.sort();
            names
        });
        for action in ["status", "disable"] {
            let output = fixture
                .command(action)
                .args(["--user-instruction", "misplaced instruction"])
                .output()
                .unwrap();
            // Clap refuses this flag before the shared service is called.
            assert_eq!(output.status.code(), Some(2));
            assert!(String::from_utf8_lossy(&output.stderr).contains("--user-instruction"));
            if let Some(original) = &original {
                assert_eq!(&fs::read(fixture.setting()).unwrap(), original);
                let mut after = fs::read_dir(fixture.database.parent().unwrap())
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name())
                    .collect::<Vec<_>>();
                after.sort();
                assert_eq!(Some(after), entries);
            } else {
                assert!(!fixture.home.exists());
            }
            assert!(!fixture.database.exists());
        }
    }
}

#[test]
fn settings_enable_requires_instruction_before_creating_any_setting_path() {
    let fixture = Fixture::new();
    for args in [vec![], vec!["--user-instruction", " \n\t"]] {
        fixture.refusal("enable", &args, "invalid_user_instruction");
        assert!(!fixture.home.exists());
    }
}

#[test]
fn settings_enable_disable_return_observed_state_and_attribution_without_sqlite() {
    let fixture = Fixture::new();
    let status = fixture.enable();
    assert_eq!(status["state"], "enabled");
    assert_eq!(
        status["user_instruction"],
        "Enable Git task sync for this project."
    );
    let stored: Value = serde_json::from_slice(&fs::read(fixture.setting()).unwrap()).unwrap();
    assert_eq!(stored["format"], "engram-task-sync-setting-v1");
    assert_eq!(stored["project_id"], "fixture/task-sync");
    assert_eq!(stored["actor_id"], "fixture-human");
    assert_eq!(stored["session_id"], "fixture-session");
    assert_eq!(stored["enabled"], true);
    assert_eq!(stored["user_instruction"], status["user_instruction"]);
    chrono::DateTime::parse_from_rfc3339(stored["changed_at"].as_str().unwrap()).unwrap();
    assert_eq!(fixture.call("status", &[]), status);
    assert!(!fixture.database.exists());
    let disabled = fixture.call("disable", &[]);
    assert_eq!(disabled["state"], "disabled");
    assert_eq!(disabled["reason"], "explicitly disabled");
    assert!(disabled["user_instruction"].is_null());
    assert_eq!(fixture.call("status", &[]), disabled);
    assert!(!fixture.database.exists());
    assert!(!fixture.root.join(".engram").exists());
}

#[test]
fn settings_status_does_not_open_or_repair_an_incompatible_database() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.database.parent().unwrap()).unwrap();
    fs::write(&fixture.database, b"deliberately not SQLite").unwrap();
    let entries = fs::read_dir(fixture.database.parent().unwrap())
        .unwrap()
        .count();
    assert_eq!(fixture.call("status", &[])["state"], "disabled");
    assert_eq!(
        fs::read(&fixture.database).unwrap(),
        b"deliberately not SQLite"
    );
    assert_eq!(
        fs::read_dir(fixture.database.parent().unwrap())
            .unwrap()
            .count(),
        entries
    );
}

#[test]
fn settings_invalid_records_are_unknown_and_an_attributed_setter_replaces_them() {
    let fixture = Fixture::new();
    fixture.enable();
    let valid: Value = serde_json::from_slice(&fs::read(fixture.setting()).unwrap()).unwrap();
    let mut cases = vec![
        b"not JSON: NEVER_ECHO_THIS".to_vec(),
        vec![b' '; 16 * 1024 + 1],
    ];
    for field in [
        "format",
        "project_id",
        "actor_id",
        "session_id",
        "changed_at",
        "user_instruction",
    ] {
        let mut changed = valid.clone();
        changed[field] = json!(" ");
        cases.push(serde_json::to_vec(&changed).unwrap());
    }
    let mut unknown = valid.clone();
    unknown["NEVER_ECHO_THIS"] = json!(true);
    cases.push(serde_json::to_vec(&unknown).unwrap());
    let mut duplicate = serde_json::to_string(&valid).unwrap();
    duplicate.insert_str(1, "\"enabled\":false,");
    cases.push(duplicate.into_bytes());
    for bytes in cases {
        fs::write(fixture.setting(), &bytes).unwrap();
        let status = fixture.call("status", &[]);
        assert_eq!(status["state"], "unknown");
        assert!(!status.to_string().contains("NEVER_ECHO_THIS"));
        assert!(status["user_instruction"].is_null());
        let disabled = fixture.call("disable", &[]);
        assert_eq!(disabled["state"], "disabled");
        assert!(disabled["user_instruction"].is_null());
        assert!(
            !fs::read(fixture.setting())
                .unwrap()
                .windows(b"NEVER_ECHO_THIS".len())
                .any(|part| part == b"NEVER_ECHO_THIS")
        );
    }
}

#[test]
fn settings_only_reads_the_final_file_and_creates_no_lock() {
    let fixture = Fixture::new();
    fixture.enable();
    let bytes = fs::read(fixture.setting()).unwrap();
    let lock_path = fixture
        .database
        .parent()
        .unwrap()
        .join("task-sync-setting.lock");
    assert!(!lock_path.exists());
    fs::remove_file(fixture.setting()).unwrap();
    fs::write(
        fixture
            .database
            .parent()
            .unwrap()
            .join("task-sync-setting-abandoned.pending"),
        bytes,
    )
    .unwrap();
    assert_eq!(
        fixture.call("status", &[])["reason"],
        "not configured (off by default)"
    );
    assert_eq!(fixture.call("disable", &[])["state"], "disabled");
}

#[test]
fn settings_ordinary_file_and_ancestor_refusals_do_not_modify_targets() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.database.parent().unwrap()).unwrap();
    fs::create_dir(fixture.setting()).unwrap();
    fs::write(fixture.setting().join("private"), b"keep").unwrap();
    assert_eq!(fixture.call("status", &[])["state"], "unknown");
    fixture.refusal("disable", &[], "setting_unreadable");
    assert_eq!(
        fs::read(fixture.setting().join("private")).unwrap(),
        b"keep"
    );
    let second = Fixture::new();
    fs::write(&second.home, b"not a directory").unwrap();
    assert_eq!(second.call("status", &[])["state"], "unknown");
    second.refusal("disable", &[], "setting_unreadable");
    assert_eq!(fs::read(&second.home).unwrap(), b"not a directory");
}

#[test]
fn settings_unrelated_private_files_are_preserved() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.database.parent().unwrap()).unwrap();
    let lock = fixture
        .database
        .parent()
        .unwrap()
        .join("task-sync-setting.lock");
    fs::write(&lock, b"unowned lock").unwrap();
    assert_eq!(fixture.enable()["state"], "enabled");
    assert_eq!(fs::read(lock).unwrap(), b"unowned lock");
    assert!(fixture.setting().exists());
}

#[cfg(unix)]
#[test]
fn settings_refuse_symbolic_files_and_ancestors_without_touching_targets() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.database.parent().unwrap()).unwrap();
    let target = fixture.owned.path().join("private-target");
    fs::write(&target, b"keep").unwrap();
    symlink(&target, fixture.setting()).unwrap();
    assert_eq!(fixture.call("status", &[])["state"], "unknown");
    fixture.refusal("disable", &[], "setting_unreadable");
    assert_eq!(fs::read(&target).unwrap(), b"keep");
    let second = Fixture::new();
    let directory = second.owned.path().join("private-directory");
    fs::create_dir(&directory).unwrap();
    symlink(&directory, &second.home).unwrap();
    assert_eq!(second.call("status", &[])["state"], "unknown");
    second.refusal("disable", &[], "setting_unreadable");
    assert_eq!(fs::read_dir(directory).unwrap().count(), 0);
}

#[test]
fn settings_setters_accept_process_session_defaults_and_refuse_invalid_attribution() {
    let fixture = Fixture::new();
    let output = Command::new(env!("CARGO_BIN_EXE_engram"))
        .current_dir(&fixture.root)
        .env_remove("ENGRAM_SESSION_ID")
        .arg("--home")
        .arg(&fixture.home)
        .arg("--project-file")
        .arg(fixture.root.join(".engram-project"))
        .args([
            "task-sync",
            "--actor-id",
            "UI user",
            "enable",
            "--user-instruction",
            "enabled in the TermAl UI by the user",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["state"], "enabled");
    let stored: Value = serde_json::from_slice(&fs::read(fixture.setting()).unwrap()).unwrap();
    assert_eq!(stored["actor_id"], "UI user");
    assert!(
        stored["session_id"]
            .as_str()
            .unwrap()
            .starts_with("local-process-v1-")
    );
    let before = fs::read(fixture.setting()).unwrap();
    let refused = fixture.command_as("disable", " ").output().unwrap();
    assert_eq!(refused.status.code(), Some(1));
    let error: Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(error["error"]["code"], "invalid_attribution");
    assert_eq!(fs::read(fixture.setting()).unwrap(), before);
}

#[test]
fn settings_project_selection_and_home_failures_have_json_refusals() {
    let fixture = Fixture::new();
    for missing_home in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_engram"));
        command.current_dir(&fixture.root).env_remove("ENGRAM_HOME");
        if !missing_home {
            command.arg("--home").arg(&fixture.home);
        }
        command
            .arg("--project-file")
            .arg(if missing_home {
                fixture.root.join(".engram-project")
            } else {
                fixture.root.join("missing-marker")
            })
            .args(["task-sync", "status", "--json"]);
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(1));
        let error: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(error["error"]["code"], "project_selection_failed");
        assert!(error["project_id"].is_null());
        assert!(error["status"].is_null());
        assert!(!fixture.home.exists());
    }
}

#[test]
fn settings_concurrent_setters_return_complete_status_and_last_value_is_readable() {
    let fixture = Fixture::new();
    fixture.enable();
    let enabling = fixture
        .command("enable")
        .args(["--user-instruction", "Enable concurrently."])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let disabling = fixture
        .command("disable")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let enabled_result = enabling.wait_with_output().unwrap();
    let disabled_result = disabling.wait_with_output().unwrap();
    for output in [enabled_result, disabled_result] {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let observed: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(matches!(
            observed["state"].as_str(),
            Some("enabled" | "disabled")
        ));
        assert_eq!(
            observed["user_instruction"].is_null(),
            observed["state"] == "disabled"
        );
    }
    let observed = fixture.call("status", &[]);
    let stored: Value = serde_json::from_slice(&fs::read(fixture.setting()).unwrap()).unwrap();
    assert_eq!(observed["state"] == "enabled", stored["enabled"] == true);
    assert!(!fixture.database.exists());
    assert!(
        !fixture
            .database
            .parent()
            .unwrap()
            .join("task-sync-setting.lock")
            .exists()
    );
}

#[cfg(windows)]
#[test]
fn settings_refuse_junction_ancestors_without_touching_targets() {
    let fixture = Fixture::new();
    let target = fixture.owned.path().join("junction-target");
    fs::create_dir(&target).unwrap();
    junction::create(&target, &fixture.home).unwrap();
    assert_eq!(fixture.call("status", &[])["state"], "unknown");
    fixture.refusal("disable", &[], "setting_unreadable");
    assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
    junction::delete(&fixture.home).unwrap();
}
