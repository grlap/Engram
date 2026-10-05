#[path = "../src/test_support.rs"]
mod test_support;

use std::{
    fmt::Write as _,
    fs,
    path::Path,
    process::{Command, Output},
};

use serde_json::Value;

fn assert_reported_cwd<'a>(value: &'a Value, expected: &Path) -> &'a Path {
    let reported = Path::new(value["error"]["details"]["cwd"].as_str().unwrap());
    assert!(reported.is_absolute());
    // current_dir may retain a Windows 8.3 alias, while canonicalize expands
    // it. Check the existing directory's identity, not its display spelling.
    assert_eq!(
        reported.canonicalize().unwrap(),
        expected.canonicalize().unwrap(),
        "reported cwd must identify the fixture directory"
    );
    reported
}

fn run(cwd: &Path, home: &Path, project_file: Option<&Path>, args: &[&str], json: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_engram"));
    command.current_dir(cwd).arg("--home").arg(home);
    if let Some(project_file) = project_file {
        command.arg("--project-file").arg(project_file);
    }
    command.arg("work").args(args);
    if json {
        command.arg("--json");
    }
    command.output().expect("run project selection fixture")
}

fn refused(output: &Output) -> Value {
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
    let value: Value = serde_json::from_slice(&output.stderr).expect("one typed refusal on stderr");
    assert_eq!(value["error"]["code"], "project_resolution_failed");
    assert_eq!(value["error"]["reminders"], serde_json::json!([]));
    assert!(
        value["error"]["details"]["selection"]
            .as_str()
            .unwrap()
            .contains("nearest .engram-project")
    );
    assert!(value["error"]["details"]["reason"].as_str().unwrap().len() > 10);
    assert_eq!(
        value["error"]["next"],
        serde_json::json!(["engram --project-file 'PROJECT_DIRECTORY/.engram-project' work next"])
    );
    value
}

/// The text line a detail must print: a string as written, with each ASCII
/// control and non-ASCII scalar escaped as its UTF-16 `\uXXXX` units; any
/// other value as JSON.
fn expected_text_detail(field: &Value) -> String {
    let Value::String(field) = field else {
        return field.to_string();
    };
    let mut expected = String::new();
    for character in field.chars() {
        if character.is_ascii() && !character.is_ascii_control() {
            expected.push(character);
        } else {
            for unit in character.encode_utf16(&mut [0; 2]) {
                let _ = write!(expected, "\\u{unit:04x}");
            }
        }
    }
    expected
}

fn text_detail<'a>(text: &'a str, key: &str) -> &'a str {
    let prefix = format!("  {key}: ");
    let lines: Vec<_> = text
        .lines()
        .filter_map(|line| line.strip_prefix(&prefix))
        .collect();
    assert_eq!(lines.len(), 1, "{text}");
    lines[0]
}

fn assert_text_details(text: &str, value: &Value) {
    for (key, field) in value["error"]["details"].as_object().unwrap() {
        let line = text_detail(text, key);
        assert!(line.is_ascii());
        assert_eq!(line, expected_text_detail(field));
    }
}

#[test]
fn every_word_refuses_explicit_missing_project_without_fallback_or_store_creation() {
    let directory = crate::test_support::temp_home().unwrap();
    fs::write(
        directory.path().join(".engram-project"),
        "do-not-select-ancestor",
    )
    .unwrap();
    let cwd = directory.path().join("wrong-cwd");
    fs::create_dir(&cwd).unwrap();
    let home = directory.path().join("uncreated-store");
    let words: &[&[&str]] = &[
        &["next"],
        &["ls"],
        &["show", "missing"],
        &["add", "Must not create"],
        &["claim", "missing"],
        &["update", "missing", "--release"],
        &["gate", "check"],
        &["note", "Must not write"],
        &["done"],
        &["handoff", "missing", "--to", "peer"],
        &["remember", "Must not remember"],
        &["memories"],
        &["forget", "missing"],
    ];
    for args in words {
        let value = refused(&run(
            &cwd,
            &home,
            Some(Path::new(".engram-project")),
            args,
            true,
        ));
        assert_eq!(value["error"]["details"]["kind"], "missing");
        let reported_cwd = assert_reported_cwd(&value, &cwd);
        assert_eq!(
            value["error"]["details"]["searched_directory"],
            reported_cwd.to_string_lossy().as_ref()
        );
        assert_eq!(
            value["error"]["details"]["project_file"],
            reported_cwd
                .join(".engram-project")
                .to_string_lossy()
                .as_ref()
        );
        let text = run(&cwd, &home, Some(Path::new(".engram-project")), args, false);
        assert_eq!(text.status.code(), Some(1));
        assert!(text.stdout.is_empty(), "{:?}", text.stdout);
        let text = String::from_utf8(text.stderr).unwrap();
        assert_text_details(&text, &value);
        // The attempted path prints as written: on Windows, with the single
        // backslashes a caller would type, never doubled by JSON framing.
        let project_file = value["error"]["details"]["project_file"].as_str().unwrap();
        if project_file.is_ascii() {
            assert_eq!(text_detail(&text, "project_file"), project_file);
        }
        if cfg!(windows) {
            assert!(project_file.contains('\\'));
            assert!(!text_detail(&text, "project_file").contains("\\\\"));
        }
        assert!(text.contains("next:\n  engram --project-file "));
        assert!(!home.exists());
        assert!(!cwd.join(".engram-project").exists());
    }
}

#[test]
fn invalid_project_files_are_typed_and_control_characters_cannot_forge_guidance() {
    let directory = crate::test_support::temp_home().unwrap();
    let home = directory.path().join("uncreated-store");
    let project_file = directory.path().join(".engram-project");
    for (bytes, kind) in [
        (b" \n\t".as_slice(), "empty"),
        (b"".as_slice(), "empty"),
        (b"\xff\xfe".as_slice(), "undecodable"),
        (b"project-\xc3".as_slice(), "undecodable"),
    ] {
        fs::write(&project_file, bytes).unwrap();
        let value = refused(&run(directory.path(), &home, None, &["ls"], true));
        assert_eq!(value["error"]["details"]["kind"], kind);
        assert_eq!(fs::read(&project_file).unwrap(), bytes);
        assert!(!home.exists());
    }
    // A directory named as the project file exists but cannot be read as one.
    let unreadable = directory.path().join("project-is-directory");
    fs::create_dir(&unreadable).unwrap();
    let value = refused(&run(
        directory.path(),
        &home,
        Some(&unreadable),
        &["ls"],
        true,
    ));
    assert_eq!(value["error"]["details"]["kind"], "unreadable");
    let absent = directory.path().join("absent").join(".engram-project");
    let value = refused(&run(directory.path(), &home, Some(&absent), &["ls"], true));
    assert_eq!(value["error"]["details"]["kind"], "missing");
    let hostile = Path::new(
        "missing\nnext:\n  injected\u{1b}[31m\u{009b}\u{202e}\u{2028}\u{2029}\u{2066}\u{e000}\u{fe0f}\u{e0100}",
    );
    let value = refused(&run(directory.path(), &home, Some(hostile), &["ls"], true));
    let reported_cwd = assert_reported_cwd(&value, directory.path());
    assert!(
        value["error"]["details"]["project_file"]
            .as_str()
            .unwrap()
            .contains('\n')
    );
    assert_eq!(
        value["error"]["details"]["project_file"],
        // The missing/hostile suffix cannot be canonicalized. It must remain
        // byte-for-byte intact after the independently checked cwd prefix.
        reported_cwd.join(hostile).to_string_lossy().as_ref()
    );
    let text = run(directory.path(), &home, Some(hostile), &["ls"], false);
    let text = String::from_utf8(text.stderr).unwrap();
    assert_text_details(&text, &value);
    assert!(!text.contains('\u{1b}'));
    assert_eq!(text.lines().filter(|line| *line == "next:").count(), 1);
    assert!(!text.lines().any(|line| line == "  injected"));
    for character in hostile
        .to_string_lossy()
        .chars()
        .filter(|ch| !ch.is_ascii())
    {
        assert!(!text.contains(character));
        assert!(
            value["error"]["details"]["reason"]
                .as_str()
                .unwrap()
                .contains(character)
        );
    }
    assert!(text.contains("\\u202e\\u2028\\u2029\\u2066\\ue000\\ufe0f\\udb40\\udd00"));
    assert!(!home.exists());
}

#[test]
fn explicit_project_file_recovers_without_rebinding_to_the_callers_cwd() {
    let directory = crate::test_support::temp_home().unwrap();
    let project_dir = directory.path().join("intended project");
    let cwd = directory.path().join("different cwd");
    fs::create_dir(&project_dir).unwrap();
    fs::create_dir(&cwd).unwrap();
    let project_file = project_dir.join(".engram-project");
    let project = engram::ProjectId(format!("project-{}", uuid::Uuid::new_v4()));
    fs::write(&project_file, format!(" {}\n", project.0)).unwrap();
    let home = directory.path().join("store");
    let initialized = Command::new(env!("CARGO_BIN_EXE_engram"))
        .current_dir(&project_dir)
        .arg("--home")
        .arg(&home)
        .arg("init")
        .output()
        .unwrap();
    assert!(
        initialized.status.success(),
        "{}",
        String::from_utf8_lossy(&initialized.stderr)
    );
    let database = engram::project_database_path(&home, &project);
    assert!(database.exists());
    let before = fs::read(&database).unwrap();
    refused(&run(
        &cwd,
        &home,
        Some(Path::new(".engram-project")),
        &["ls"],
        true,
    ));
    assert_eq!(fs::read(&database).unwrap(), before);
    let recovered = run(&cwd, &home, Some(&project_file), &["ls"], true);
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    let value: Value = serde_json::from_slice(&recovered.stdout).unwrap();
    assert_eq!(value["total"], 0);
    assert!(!cwd.join(".engram-project").exists());
    // An explicit relative file still resolves from cwd, not from ENGRAM_HOME.
    let relative = Path::new("..")
        .join("intended project")
        .join(".engram-project");
    assert!(
        run(&cwd, &home, Some(&relative), &["ls"], true)
            .status
            .success()
    );
}

fn initialize_fixture(root: &Path, home: &Path, project: &engram::ProjectId) {
    fs::write(root.join(".engram-project"), &project.0).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_engram"))
        .current_dir(root)
        .env_remove("ENGRAM_HOST_PATH_POLICY")
        .arg("--home")
        .arg(home)
        .args([
            "init",
            "--required-assurance",
            "advisory",
            "--authorized-by",
            "fixture",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn nested_directory_selects_nearest_project_and_explicit_override() {
    let directory = test_support::temp_home().unwrap();
    let home = directory.path().join("home");
    let outer = engram::ProjectId(format!("outer-{}", uuid::Uuid::new_v4()));
    initialize_fixture(directory.path(), &home, &outer);
    let nested = directory.path().join("nested");
    let cwd = nested.join("src").join("deep");
    fs::create_dir_all(&cwd).unwrap();
    let output = run(&cwd, &home, None, &["add", "outer item"], true);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let inner = engram::ProjectId(format!("inner-{}", uuid::Uuid::new_v4()));
    initialize_fixture(&nested, &home, &inner);
    let output = run(&cwd, &home, None, &["ls"], true);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["total"],
        0
    );
    for explicit in [
        directory.path().join(".engram-project"),
        Path::new("../../..").join(".engram-project"),
    ] {
        let output = run(&cwd, &home, Some(&explicit), &["ls"], true);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap()["total"],
            1
        );
    }
    assert!(!cwd.join(".engram-project").exists());
    let output = Command::new(env!("CARGO_BIN_EXE_engram"))
        .current_dir(&cwd)
        .env_remove("ENGRAM_HOST_PATH_POLICY")
        .arg("--home")
        .arg(&home)
        .args(["readiness", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let readiness: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(readiness["project_id"], inner.0);
    let expected_policy = engram::probe_host_path_policy(&nested.join(".engram-project")).unwrap();
    assert_eq!(
        readiness["host_path_policy"]["resolved"],
        engram::describe_host_path_policy(expected_policy)
    );
}

#[test]
fn unreadable_nearest_marker_does_not_select_the_valid_outer_project() {
    let directory = test_support::temp_home().unwrap();
    let home = directory.path().join("uncreated-home");
    fs::write(directory.path().join(".engram-project"), "outer").unwrap();
    let cwd = directory.path().join("child");
    fs::create_dir_all(cwd.join(".engram-project")).unwrap();
    let value = refused(&run(&cwd, &home, None, &["ls"], true));
    assert_eq!(value["error"]["details"]["kind"], "unreadable");
    assert_eq!(
        value["error"]["details"]["project_file"],
        cwd.join(".engram-project").to_string_lossy().as_ref()
    );
    assert!(!home.exists());
}

#[test]
fn linked_worktree_selection_crosses_git_boundary_only_without_its_marker() {
    let directory = test_support::temp_home().unwrap();
    let root = directory.path();
    let home = root.join("home");
    let project = engram::ProjectId(format!("linked-{}", uuid::Uuid::new_v4()));
    initialize_fixture(root, &home, &project);
    let config = root.join("empty.gitconfig");
    fs::write(&config, "").unwrap();
    let git = |args: &[&str]| {
        let mut command = Command::new("git");
        for name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_IMPLICIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_PREFIX",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_SHALLOW_FILE",
            "GIT_NO_REPLACE_OBJECTS",
            "GIT_REPLACE_REF_BASE",
            "GIT_NAMESPACE",
            "GIT_CONFIG",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
        ] {
            command.env_remove(name);
        }
        let output = command
            .current_dir(root)
            .env("GIT_CEILING_DIRECTORIES", root.parent().unwrap())
            .env("GIT_GRAFT_FILE", &config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", &config)
            .env("GIT_TERMINAL_PROMPT", "0")
            .args([
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&["add", ".engram-project"]);
    git(&["commit", "-q", "-m", "fixture"]);
    git(&["worktree", "add", "-q", "-b", "linked-fixture", "linked"]);
    let linked = root.join("linked");
    let cwd = linked.join("src").join("deep");
    fs::create_dir_all(&cwd).unwrap();
    assert!(linked.join(".git").is_file());
    for selected in [&linked, root] {
        let output = Command::new(env!("CARGO_BIN_EXE_engram"))
            .current_dir(&cwd)
            .env_remove("ENGRAM_HOST_PATH_POLICY")
            .arg("--home")
            .arg(&home)
            .args(["doctor", "--check-landings", "--json"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["repository_problem"], Value::Null);
        // Compare the implicit selection with an explicit selection of the expected marker.
        let explicit = Command::new(env!("CARGO_BIN_EXE_engram"))
            .current_dir(&cwd)
            .env_remove("ENGRAM_HOST_PATH_POLICY")
            .arg("--home")
            .arg(&home)
            .arg("--project-file")
            .arg(selected.join(".engram-project"))
            .args(["doctor", "--check-landings", "--json"])
            .output()
            .unwrap();
        assert!(explicit.status.success());
        let expected: Value = serde_json::from_slice(&explicit.stdout).unwrap();
        assert!(report["repository"].is_string());
        assert_eq!(report["repository"], expected["repository"]);
        assert_eq!(
            Path::new(report["repository"].as_str().unwrap())
                .canonicalize()
                .unwrap(),
            selected.canonicalize().unwrap()
        );
        if selected == linked {
            fs::remove_file(linked.join(".engram-project")).unwrap();
        }
    }
    let explicit_repo = root.join("not-a-repository");
    fs::create_dir(&explicit_repo).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_engram"))
        .current_dir(&cwd)
        .env_remove("ENGRAM_HOST_PATH_POLICY")
        .arg("--home")
        .arg(&home)
        .args(["doctor", "--check-landings", "--repo"])
        .arg(&explicit_repo)
        .arg("--json")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report["repository_problem"].is_string());
    assert_eq!(
        Path::new(report["repository"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        explicit_repo.canonicalize().unwrap()
    );
}
