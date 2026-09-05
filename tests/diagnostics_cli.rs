use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use engram::{
    CanonicalObject,
    storage::{running_schema_reference, store_schema_reference},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_engram"))
        .arg("--home")
        .arg(home)
        .args(args)
        .output()
        .expect("run engram")
}

fn success(home: &Path, args: &[&str]) -> Output {
    let output = run(home, args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn diagnosis(home: &Path) -> Value {
    serde_json::from_slice(&success(home, &["doctor", "--json"]).stdout).unwrap()
}

#[test]
fn version_next_and_doctor_share_runtime_identity_across_processes() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path();
    success(home, &["init"]);
    let doctor = diagnosis(home);
    let executable = fs::read(env!("CARGO_BIN_EXE_engram")).unwrap();
    let build = json!({
        "package_version": env!("CARGO_PKG_VERSION"),
        "executable_sha256": format!("{:x}", Sha256::digest(executable)),
        "schema_reference": running_schema_reference().unwrap(),
    });
    let fingerprint = CanonicalObject::freeze(&build).unwrap().hash().clone();
    assert_eq!(doctor["build"], build);
    assert_eq!(doctor["build_fingerprint"], json!(fingerprint));
    assert_eq!(diagnosis(home)["build_fingerprint"], json!(fingerprint));
    let version = success(home, &["--version"]);
    assert_eq!(version.stdout, success(home, &["-V"]).stdout);
    assert_eq!(
        String::from_utf8(version.stdout).unwrap().trim(),
        format!(
            "engram {} build {} (exe {}, schema {})",
            env!("CARGO_PKG_VERSION"),
            &fingerprint.as_str()[..12],
            &build["executable_sha256"].as_str().unwrap()[..12],
            &build["schema_reference"].as_str().unwrap()[..12],
        )
    );
    for verbose in [false, true] {
        let mut args = vec![
            "work",
            "--actor-id",
            "identity",
            "--session-id",
            "identity",
            "next",
        ];
        if verbose {
            args.push("--verbose");
        }
        let text = String::from_utf8(success(home, &args).stdout).unwrap();
        assert_eq!(
            text.trim().lines().last(),
            Some(format!("build: {}", &fingerprint.as_str()[..12]).as_str())
        );
        assert_eq!(text.matches("build:").count(), 1);
        args.push("--json");
        let output = success(home, &args);
        let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(receipt["build_fingerprint"], json!(fingerprint));
        assert_eq!(
            String::from_utf8(output.stdout)
                .unwrap()
                .matches("build_fingerprint")
                .count(),
            1
        );
    }
    let ls: Value = serde_json::from_slice(
        &success(
            home,
            &[
                "work",
                "--actor-id",
                "identity",
                "--session-id",
                "identity",
                "ls",
                "--json",
            ],
        )
        .stdout,
    )
    .unwrap();
    assert!(ls.get("build_fingerprint").is_none());
    for mode in ["--recover-policy", "--repair-projections"] {
        let report: Value =
            serde_json::from_slice(&success(home, &["doctor", mode, "--json"]).stdout).unwrap();
        assert_eq!(report["build"], build);
        assert_eq!(report["build_fingerprint"], json!(fingerprint));
    }
}

#[test]
fn doctor_cli_refusals_are_json_and_leave_the_store_unchanged() {
    for (damage, code) in [
        (
            "DROP INDEX memory_heads_scope",
            "projection_repair_required",
        ),
        ("DROP TABLE object_fts", "projection_repair_required"),
        (
            "DROP INDEX objects_work_event_work_id",
            "projection_repair_required",
        ),
        (
            "ALTER TABLE objects ADD COLUMN different_build TEXT",
            "different_build_schema",
        ),
        (
            "UPDATE control_policy_versions SET policy_json = X'7B7D'",
            "corrupt_store",
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path();
        success(home, &["init"]);
        let healthy = diagnosis(home);
        let database = Path::new(healthy["database"].as_str().unwrap());
        let connection = rusqlite::Connection::open(database).unwrap();
        connection.execute_batch(damage).unwrap();
        drop(connection);
        let before = fs::read(database).unwrap();
        let refused = run(home, &["doctor", "--json"]);
        assert!(!refused.status.success(), "{damage}");
        let report: Value =
            serde_json::from_slice(&refused.stdout).expect("one complete JSON refusal on stdout");
        assert_eq!(report["healthy"], false);
        assert_eq!(report["code"], code);
        assert_eq!(report["database"], healthy["database"]);
        assert_eq!(report["phase"], "open");
        assert_eq!(report["build"], healthy["build"]);
        assert_eq!(report["build_fingerprint"], healthy["build_fingerprint"]);
        assert_eq!(fs::read(database).unwrap(), before);
        match code {
            "projection_repair_required" => {
                assert_eq!(report["remedy"], "engram doctor --repair-projections");
                assert_eq!(report["scope"], json!(["indexes", "triggers", "fts"]));
            }
            "different_build_schema" => {
                assert_eq!(
                    report["store_schema_reference"],
                    json!(store_schema_reference(database).unwrap())
                );
                assert_eq!(report["running"], report["build"]);
                assert_ne!(
                    report["store_schema_reference"],
                    report["running"]["schema_reference"]
                );
            }
            _ => assert!(!report["findings"].as_array().unwrap().is_empty()),
        }
        let refused_text = run(home, &["doctor"]);
        assert!(!refused_text.status.success());
        let text = String::from_utf8(refused_text.stdout).unwrap();
        for (key, value) in report.as_object().unwrap() {
            assert!(
                text.lines().any(|line| line == format!("{key}: {value}")),
                "missing {key}"
            );
        }
        assert_eq!(fs::read(database).unwrap(), before);
    }
}

#[test]
fn healthy_store_path_policy_refusal_is_actionable_and_not_corruption() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path();
    success(home, &["--host-path-policy", "case_sensitive", "init"]);
    let baseline: Value = serde_json::from_slice(
        &success(
            home,
            &["--host-path-policy", "case_sensitive", "doctor", "--json"],
        )
        .stdout,
    )
    .unwrap();
    let database = Path::new(baseline["database"].as_str().unwrap());
    let before = fs::read(database).unwrap();
    let expected_policy = engram::HostPathPolicy {
        case_fold_paths: true,
        ..engram::HostPathPolicy::host_default()
    };
    let error = engram::SqliteStore::open_with_host_path_identity(database, Some(expected_policy))
        .err()
        .unwrap();
    let output = run(
        home,
        &["--host-path-policy", "case_fold", "doctor", "--json"],
    );
    assert!(!output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["healthy"], false);
    assert_eq!(value["code"], "store_open_refused");
    assert_eq!(value["kind"], "path_policy");
    assert_eq!(value["reason"], error.to_string());
    let remedy = value["remedy"].as_str().unwrap();
    assert!(remedy.contains("Recorded policy: case_sensitive"));
    assert!(remedy.contains("requested policy: case_fold"));
    assert!(remedy.ends_with("--host-path-policy case_sensitive"));
    assert!(value.get("findings").is_none());
    assert_eq!(fs::read(database).unwrap(), before);
    success(
        home,
        &["--host-path-policy", "case_sensitive", "doctor", "--json"],
    );
}
