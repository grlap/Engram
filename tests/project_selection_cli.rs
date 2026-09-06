#[path = "../src/test_support.rs"]
mod test_support;

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use serde_json::Value;

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
    assert!(output.stdout.is_empty());
    let value: Value = serde_json::from_slice(&output.stderr).expect("one typed refusal on stderr");
    assert_eq!(value["error"]["code"], "project_resolution_failed");
    assert_eq!(value["error"]["reminders"], serde_json::json!([]));
    assert!(
        value["error"]["details"]["selection"]
            .as_str()
            .unwrap()
            .contains("cwd-based")
    );
    assert!(value["error"]["details"]["reason"].as_str().unwrap().len() > 10);
    assert_eq!(
        value["error"]["next"],
        serde_json::json!(["engram --project-file 'PROJECT_DIRECTORY/.engram-project' work next"])
    );
    value
}

fn assert_text_details(text: &str, value: &Value) {
    for (key, field) in value["error"]["details"].as_object().unwrap() {
        let prefix = format!("  {key}: ");
        let lines: Vec<_> = text
            .lines()
            .filter_map(|line| line.strip_prefix(&prefix))
            .collect();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].is_ascii());
        assert_eq!(serde_json::from_str::<Value>(lines[0]).unwrap(), *field);
    }
}

#[test]
fn every_word_refuses_missing_cwd_project_without_search_or_store_creation() {
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
        let value = refused(&run(&cwd, &home, None, args, true));
        assert_eq!(value["error"]["details"]["kind"], "unreadable");
        assert_eq!(
            value["error"]["details"]["cwd"],
            cwd.to_string_lossy().as_ref()
        );
        assert_eq!(
            value["error"]["details"]["searched_directory"],
            cwd.to_string_lossy().as_ref()
        );
        assert_eq!(
            value["error"]["details"]["project_file"],
            cwd.join(".engram-project").to_string_lossy().as_ref()
        );
        let text = run(&cwd, &home, None, args, false);
        assert_eq!(text.status.code(), Some(1));
        assert!(text.stdout.is_empty());
        let text = String::from_utf8(text.stderr).unwrap();
        assert_text_details(&text, &value);
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
        (b"\xff\xfe".as_slice(), "unreadable"),
    ] {
        fs::write(&project_file, bytes).unwrap();
        let value = refused(&run(directory.path(), &home, None, &["ls"], true));
        assert_eq!(value["error"]["details"]["kind"], kind);
        assert_eq!(fs::read(&project_file).unwrap(), bytes);
        assert!(!home.exists());
    }
    let unreadable = directory.path().join("project-is-directory");
    fs::create_dir(&unreadable).unwrap();
    refused(&run(
        directory.path(),
        &home,
        Some(&unreadable),
        &["ls"],
        true,
    ));
    let hostile = Path::new(
        "missing\nnext:\n  injected\u{1b}[31m\u{009b}\u{202e}\u{2028}\u{2029}\u{2066}\u{e000}\u{fe0f}\u{e0100}",
    );
    let value = refused(&run(directory.path(), &home, Some(hostile), &["ls"], true));
    assert!(
        value["error"]["details"]["project_file"]
            .as_str()
            .unwrap()
            .contains('\n')
    );
    assert_eq!(
        value["error"]["details"]["project_file"],
        directory.path().join(hostile).to_string_lossy().as_ref()
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
    refused(&run(&cwd, &home, None, &["ls"], true));
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
