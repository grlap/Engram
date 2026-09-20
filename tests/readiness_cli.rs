#[path = "../src/test_support.rs"]
mod test_support;

use engram::{
    ProjectId, SqliteStore, describe_host_path_policy, parse_host_path_policy,
    project_database_path,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_engram"))
        .env_remove("ENGRAM_HOME")
        .env_remove("ENGRAM_HOST_PATH_POLICY")
        .arg("--home")
        .arg(root.join("home"))
        .arg("--project-file")
        .arg(root.join(".engram-project"))
        .args(args)
        .output()
        .unwrap()
}

fn setup(root: &Path) -> std::path::PathBuf {
    fs::write(root.join(".engram-project"), "readiness-fixture\n").unwrap();
    let result = run(root, &["init"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    project_database_path(&root.join("home"), &ProjectId("readiness-fixture".into()))
}

fn receipt(output: &Output, ready: bool) -> Value {
    assert_eq!(
        output.status.code(),
        Some(i32::from(!ready)),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["scope"], "readiness");
    assert_eq!(value["ready"], ready);
    assert_eq!(value["full_audit"], "not_run");
    assert_eq!(value["mutation_enabled"], false);
    assert!(value.get("healthy").is_none());
    assert!(value.get("build").is_some(), "{value}");
    assert!(value.get("build_fingerprint").is_some(), "{value}");
    if !ready {
        for field in ["code", "phase", "reason", "remedy"] {
            assert!(
                value[field].as_str().is_some_and(|s| !s.is_empty()),
                "{value}"
            );
        }
        assert!(value.get("control").is_none());
    }
    value
}

#[test]
fn readiness_reports_scoped_identity_and_policy_without_mutation() {
    let root = test_support::temp_home().unwrap();
    let database = setup(root.path());
    let before = fs::read(&database).unwrap();
    let value = receipt(&run(root.path(), &["readiness", "--json"]), true);
    assert_eq!(fs::read(&database).unwrap(), before);
    assert_eq!(value["project_id"], "readiness-fixture");
    assert!(Path::new(value["database"].as_str().unwrap()).is_absolute());
    assert_eq!(
        fs::canonicalize(value["database"].as_str().unwrap()).unwrap(),
        fs::canonicalize(&database).unwrap()
    );
    assert_eq!(value["work_schema_version"], 1);
    assert_eq!(value["host_path_policy"]["status"], "matched");
    let policy: Value =
        serde_json::from_slice(&run(root.path(), &["control-policy", "show"]).stdout).unwrap();
    assert_eq!(value["control"], policy);
    assert!(value["control"].get("sessions").is_none());
}

#[test]
fn readiness_refuses_missing_store_without_creating_it() {
    let root = test_support::temp_home().unwrap();
    fs::write(root.path().join(".engram-project"), "not-initialized").unwrap();
    let value = receipt(&run(root.path(), &["readiness", "--json"]), false);
    assert_eq!(value["code"], "store_not_initialized");
    assert_eq!(value["phase"], "open");
    assert!(!root.path().join("home").exists());
}

#[test]
fn readiness_resolution_refusals_are_structured() {
    let root = test_support::temp_home().unwrap();
    let value = receipt(&run(root.path(), &["readiness", "--json"]), false);
    assert_eq!(value["code"], "project_resolution_failed");
    assert_eq!(value["phase"], "resolve");
    assert!(value["database"].is_null());
    fs::write(root.path().join(".engram-project"), "project").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_engram"))
        .env_remove("ENGRAM_HOME")
        .arg("--project-file")
        .arg(root.path().join(".engram-project"))
        .args(["readiness", "--json"])
        .output()
        .unwrap();
    let value = receipt(&output, false);
    assert_eq!(value["code"], "home_required");
    assert_eq!(value["phase"], "resolve");
}

#[test]
fn readiness_preserves_schema_policy_and_projection_refusals() {
    for (sql, code) in [
        (
            "CREATE TABLE unsupported(value TEXT)",
            "different_build_schema",
        ),
        (
            "UPDATE control_policy_versions SET policy_json = X'7B7D'",
            "corrupt_store",
        ),
        (
            "UPDATE control_policy_state SET required_assurance = 'advisory'",
            "corrupt_store",
        ),
        (
            "DROP INDEX objects_memory_assertion_version",
            "projection_repair_required",
        ),
    ] {
        let root = test_support::temp_home().unwrap();
        let database = setup(root.path());
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection.execute_batch(sql).unwrap();
        drop(connection);
        let before = fs::read(&database).unwrap();
        let value = receipt(&run(root.path(), &["readiness", "--json"]), false);
        assert_eq!(value["code"], code, "{value}");
        assert_eq!(value["phase"], "open");
        if code == "projection_repair_required" {
            assert_eq!(
                value["detail"]["scope"],
                json!(["indexes", "triggers", "fts"])
            );
        }
        assert_eq!(fs::read(&database).unwrap(), before);
    }
}

#[test]
fn readiness_is_not_a_work_history_audit() {
    let root = test_support::temp_home().unwrap();
    let database = setup(root.path());
    assert!(
        run(
            root.path(),
            &[
                "work",
                "--actor-id",
                "test",
                "--session-id",
                "test",
                "add",
                "Original title"
            ]
        )
        .status
        .success()
    );
    let connection = rusqlite::Connection::open(&database).unwrap();
    assert_eq!(
        connection
            .execute("UPDATE work_items SET priority = (priority + 1) % 5", [])
            .unwrap(),
        1
    );
    drop(connection);
    receipt(&run(root.path(), &["readiness", "--json"]), true);
    let output = run(root.path(), &["doctor", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["healthy"], false);
    assert!(
        !value["invalid"]["work_records"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn readiness_discloses_unbound_path_without_binding_and_refuses_mismatch() {
    // Both identities discriminate an accidental host-default description on
    // every platform, independent of the filesystem backing this fixture.
    for (requested, mismatched) in [
        ("case_fold", "case_sensitive"),
        ("case_sensitive", "case_fold"),
    ] {
        assert_path_descriptions_and_mismatch(requested, mismatched);
    }
}

fn assert_path_descriptions_and_mismatch(requested: &str, mismatched: &str) {
    let root = test_support::temp_home().unwrap();
    fs::write(root.path().join(".engram-project"), "readiness-fixture").unwrap();
    let database = project_database_path(
        &root.path().join("home"),
        &ProjectId("readiness-fixture".into()),
    );
    fs::create_dir_all(database.parent().unwrap()).unwrap();
    drop(SqliteStore::open_unresolved(&database).unwrap());
    assert!(
        SqliteStore::readiness(&database, None)
            .unwrap()
            .stored_host_path_policy
            .is_none()
    );
    let before = fs::read(&database).unwrap();
    let description = describe_host_path_policy(parse_host_path_policy(requested).unwrap());
    let value = receipt(
        &run(
            root.path(),
            &["--host-path-policy", requested, "readiness", "--json"],
        ),
        true,
    );
    assert_eq!(value["host_path_policy"]["status"], "unbound");
    assert_eq!(value["host_path_policy"]["stored"], Value::Null);
    assert_eq!(value["host_path_policy"]["resolved"], description);
    assert_eq!(fs::read(&database).unwrap(), before);
    assert!(
        run(root.path(), &["--host-path-policy", requested, "init"])
            .status
            .success()
    );
    assert!(
        SqliteStore::readiness(&database, None)
            .unwrap()
            .stored_host_path_policy
            .is_some()
    );
    let value = receipt(
        &run(
            root.path(),
            &["--host-path-policy", requested, "readiness", "--json"],
        ),
        true,
    );
    assert_eq!(value["host_path_policy"]["status"], "matched");
    assert_eq!(value["host_path_policy"]["stored"], description);
    assert_eq!(value["host_path_policy"]["resolved"], description);
    let value = receipt(
        &run(
            root.path(),
            &["--host-path-policy", mismatched, "readiness", "--json"],
        ),
        false,
    );
    assert_eq!(value["code"], "store_open_refused");
    assert_eq!(value["kind"], json!("path_policy"));
    assert_eq!(value["phase"], "open");
}

#[test]
fn readiness_text_is_scoped_and_does_not_claim_health() {
    let root = test_support::temp_home().unwrap();
    setup(root.path());
    let output = run(root.path(), &["readiness"]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("scope: \"readiness\""));
    assert!(text.contains("full_audit: \"not_run\""));
    assert!(!text.contains("healthy"));
}

#[test]
fn readiness_and_doctor_text_frame_unicode_without_changing_json() {
    let root = test_support::temp_home().unwrap();
    let hostile = "diagnostics-A\u{009b}\u{202e}\u{034f}\u{fe0f}\u{2028}\u{2029}-Z";
    fs::write(root.path().join(".engram-project"), hostile).unwrap();
    assert!(run(root.path(), &["init"]).status.success());
    let database = project_database_path(&root.path().join("home"), &ProjectId(hostile.into()));
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch("DROP INDEX memory_heads_scope")
        .unwrap();
    drop(connection);

    for command in ["readiness", "doctor"] {
        let output = run(root.path(), &[command]);
        assert_eq!(output.status.code(), Some(1));
        let text = String::from_utf8(output.stdout).unwrap();
        for character in [
            '\u{009b}', '\u{202e}', '\u{034f}', '\u{fe0f}', '\u{2028}', '\u{2029}',
        ] {
            assert!(!text.contains(character), "{command}: raw {character:?}");
        }
        let expected = engram::terminal_error_command(&serde_json::to_string(hostile).unwrap());
        assert!(
            text.lines()
                .any(|line| line == format!("project_id: {expected}"))
        );
        let output = run(root.path(), &[command, "--json"]);
        assert_eq!(output.status.code(), Some(1));
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["project_id"], hostile);
        assert_eq!(value["code"], "projection_repair_required");
    }
}
