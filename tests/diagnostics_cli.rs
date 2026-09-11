#[path = "../src/test_support.rs"]
mod test_support;

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
fn doctor_discloses_the_verified_snapshot_in_json_and_text() {
    let directory = crate::test_support::temp_home().unwrap();
    let home = directory.path();
    success(home, &["init"]);
    let empty = diagnosis(home);
    assert_eq!(
        empty["verified_snapshot"]["project_feed_head"]["position"],
        0
    );
    success(
        home,
        &[
            "work",
            "--actor-id",
            "snapshot",
            "--session-id",
            "snapshot",
            "add",
            "Snapshot disclosure",
        ],
    );
    let report = diagnosis(home);
    let connection = rusqlite::Connection::open(report["database"].as_str().unwrap()).unwrap();
    let objects: i64 = connection
        .query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))
        .unwrap();
    let position: i64 = connection
        .query_row(
            "SELECT position FROM work_feed_heads WHERE feed_kind = 'project' AND feed_id = ?1",
            [report["project_id"].as_str().unwrap()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(position > 0);
    assert_eq!(
        report["verified_snapshot"],
        json!({
            "object_count": objects,
            "project_feed_head": {
                "feed": {"kind": "project", "id": report["project_id"]},
                "position": position,
            },
        })
    );
    let text = String::from_utf8(success(home, &["doctor"]).stdout).unwrap();
    assert!(text.contains(&format!(
        "Verified snapshot: {objects} immutable object(s); selected project feed head position {position}"
    )));
}

#[test]
fn version_next_and_doctor_share_runtime_identity_across_processes() {
    let directory = crate::test_support::temp_home().unwrap();
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
        let footer = text.trim().lines().last().unwrap();
        let instant = footer
            .strip_prefix(&format!(
                "build: {}; read cut: project 0 observed_at ",
                &fingerprint.as_str()[..12]
            ))
            .unwrap();
        chrono::DateTime::parse_from_rfc3339(instant).unwrap();
        assert_eq!(text.matches("build:").count(), 1);
        args.push("--json");
        let output = success(home, &args);
        let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(receipt["build_fingerprint"], json!(fingerprint));
        assert_eq!(receipt["read_cut"]["project_position"], 0);
        chrono::DateTime::parse_from_rfc3339(receipt["read_cut"]["observed_at"].as_str().unwrap())
            .unwrap();
        assert!(receipt.get("context_generation").is_none());
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

fn assert_unknown_schema_refusal(
    report: &Value,
    database: &Path,
    text_result: &Output,
    json_result: &Output,
) {
    assert!(
        report["remedy"]
            .as_str()
            .unwrap()
            .contains("Use the Engram build that created this store")
    );
    assert!(report.get("findings").is_none());
    let product_text = format!(
        "{}{}{}{}",
        report["remedy"].as_str().unwrap_or_default(),
        report["reason"].as_str().unwrap_or_default(),
        String::from_utf8_lossy(&text_result.stderr),
        String::from_utf8_lossy(&json_result.stderr)
    );
    for forbidden in [
        "corrupt_store",
        "durable state is invalid",
        "invalid data",
        "restore",
        "re-initialize",
    ] {
        assert!(!product_text.contains(forbidden), "{product_text}");
    }
    assert_eq!(
        report["store_schema_reference"],
        json!(store_schema_reference(database).unwrap())
    );
}

fn schema_refusal_fixtures() -> [(&'static str, &'static str); 8] {
    [
        (
            "CREATE INDEX extra_objects_index ON objects(object_kind)",
            "different_build_schema",
        ),
        (
            "CREATE INDEX work_extra_index ON work_items(priority)",
            "different_build_schema",
        ),
        (
            "CREATE INDEX object_fts_extra ON objects(object_kind)",
            "different_build_schema",
        ),
        (
            "CREATE INDEX work_catalog_fts_extra ON work_items(priority)",
            "different_build_schema",
        ),
        (
            "CREATE TRIGGER object_fts_extra_trigger AFTER INSERT ON objects BEGIN SELECT 1; END",
            "different_build_schema",
        ),
        (
            "CREATE TABLE work_catalog_fts_extra_table(value TEXT)",
            "different_build_schema",
        ),
        (
            "CREATE INDEX sqliteX_extra ON objects(object_kind)",
            "different_build_schema",
        ),
        (
            "UPDATE control_policy_versions SET policy_json = X'7B7D'",
            "corrupt_store",
        ),
    ]
}

#[test]
fn schema_refusal_cli_keeps_unknown_indexes_distinct_from_corruption() {
    for (fixture_sql, expected_code) in schema_refusal_fixtures() {
        assert_schema_refusal_cli_without_mutation(fixture_sql, expected_code);
    }
}

fn assert_schema_refusal_cli_without_mutation(fixture_sql: &str, expected_code: &str) {
    let directory = crate::test_support::temp_home().unwrap();
    let home_path = directory.path().join("restore-invalid data-corrupt_store");
    fs::create_dir(&home_path).unwrap();
    let home = home_path.as_path();
    success(home, &["init"]);
    success(
        home,
        &[
            "work",
            "--actor-id",
            "schema-refusal",
            "--session-id",
            "schema-refusal",
            "add",
            "Preserved work",
        ],
    );
    let healthy = diagnosis(home);
    let database = Path::new(healthy["database"].as_str().unwrap());
    let connection = rusqlite::Connection::open(database).unwrap();
    connection.execute_batch(fixture_sql).unwrap();
    drop(connection);
    let before = fs::read(database).unwrap();

    let next = run(
        home,
        &[
            "work",
            "--actor-id",
            "schema-refusal",
            "--session-id",
            "schema-refusal",
            "next",
            "--peek",
        ],
    );
    assert!(!next.status.success());
    assert_eq!(fs::read(database).unwrap(), before);
    if expected_code == "different_build_schema" {
        let text = String::from_utf8(next.stderr).unwrap();
        assert!(
            text.contains("use the Engram build that owns this store"),
            "{text}"
        );
        assert!(!text.contains("restore"), "{text}");
        assert!(!text.contains("re-initialize"), "{text}");
        assert!(!text.contains("invalid data"), "{text}");
    }

    for repair in [false, true] {
        let mut args = vec!["doctor"];
        if repair {
            args.push("--repair-projections");
        }
        let text_result = run(home, &args);
        assert!(!text_result.status.success());
        assert_eq!(fs::read(database).unwrap(), before);
        args.push("--json");
        let json_result = run(home, &args);
        assert!(!json_result.status.success());
        assert_eq!(fs::read(database).unwrap(), before);
        let report: Value = serde_json::from_slice(&json_result.stdout).unwrap();
        assert_eq!(report["code"], expected_code);
        assert_eq!(
            report["phase"],
            if repair { "projection_repair" } else { "open" }
        );
        assert_eq!(report["healthy"], false);
        assert_eq!(report["mutation_enabled"], false);
        assert_eq!(report["database"], healthy["database"]);
        if expected_code == "different_build_schema" {
            assert_unknown_schema_refusal(&report, database, &text_result, &json_result);
        } else {
            assert!(!report["findings"].as_array().unwrap().is_empty());
        }
        let text = String::from_utf8(text_result.stdout).unwrap();
        for (key, value) in report.as_object().unwrap() {
            assert!(
                text.lines().any(|line| line == format!("{key}: {value}")),
                "missing {key}"
            );
        }
    }
}

#[test]
fn schema_object_namespace_collisions_refuse_through_cli() {
    for sql in [
        "CREATE TRIGGER object_fts_data AFTER INSERT ON objects BEGIN SELECT 1; END",
        "CREATE TRIGGER work_catalog_fts_data AFTER INSERT ON work_items BEGIN SELECT 1; END",
        "CREATE TRIGGER objects_memory_assertion_version AFTER INSERT ON objects BEGIN SELECT 1; END",
        "CREATE TRIGGER objects_work_event_work_id AFTER INSERT ON objects BEGIN SELECT 1; END",
        "CREATE TRIGGER project_memory_state AFTER INSERT ON objects BEGIN SELECT 1; END",
        "CREATE INDEX work_feed_entries_require_work_id ON work_items(priority)",
    ] {
        assert_schema_refusal_cli_without_mutation(sql, "different_build_schema");
    }
}

#[test]
fn missing_work_schema_metadata_refuses_through_cli() {
    assert_schema_refusal_cli_without_mutation(
        "DROP TABLE work_schema_metadata",
        "different_build_schema",
    );
}

#[test]
fn orphan_fts_schema_shadows_refuse_through_cli() {
    for table in ["object_fts", "work_catalog_fts"] {
        for parent in [
            String::new(),
            format!("CREATE TABLE {table}(value TEXT);"),
            format!("CREATE VIEW {table} AS SELECT 'preserved' AS value;"),
        ] {
            let sql = format!(
                "DROP TABLE {table}; {parent}
                 CREATE TABLE {table}_data(value TEXT);
                 INSERT INTO {table}_data VALUES ('preserved');"
            );
            assert_schema_refusal_cli_without_mutation(&sql, "different_build_schema");
        }
        assert_schema_refusal_cli_without_mutation(
            &format!("DROP TABLE {table}; CREATE VIRTUAL TABLE {table} USING fts5(value);"),
            "different_build_schema",
        );
    }
}

#[test]
fn non_table_fts_schema_shadows_refuse_through_cli() {
    for table in ["object_fts", "work_catalog_fts"] {
        for object in [
            format!("CREATE INDEX {table}_data ON objects(object_kind);"),
            format!("CREATE TRIGGER {table}_data AFTER INSERT ON objects BEGIN SELECT 1; END;"),
            format!("CREATE VIEW {table}_data AS SELECT 'preserved' AS value;"),
        ] {
            assert_schema_refusal_cli_without_mutation(
                &format!("DROP TABLE {table}; {object}"),
                "different_build_schema",
            );
        }
    }
}

#[test]
fn sqlite_name_lookalike_cli_refuses_foreign_schema_without_mutation() {
    let directory = crate::test_support::temp_home().unwrap();
    let home = directory.path();
    success(home, &["init"]);
    let healthy = diagnosis(home);
    let database = Path::new(healthy["database"].as_str().unwrap());
    fs::write(database, []).unwrap();
    let connection = rusqlite::Connection::open(database).unwrap();
    connection
        .execute_batch("CREATE TABLE sqliteX_extra(value TEXT); INSERT INTO sqliteX_extra VALUES ('preserved')")
        .unwrap();
    drop(connection);
    let before = fs::read(database).unwrap();
    for repair in [true, false] {
        let mut args = vec!["doctor"];
        if repair {
            args.push("--repair-projections");
        }
        let text_result = run(home, &args);
        assert!(!text_result.status.success());
        assert_eq!(fs::read(database).unwrap(), before);
        args.push("--json");
        let json_result = run(home, &args);
        assert!(!json_result.status.success());
        assert_eq!(fs::read(database).unwrap(), before);
        let report: Value = serde_json::from_slice(&json_result.stdout).unwrap();
        assert_eq!(report["code"], "different_build_schema");
        assert_eq!(report["healthy"], false);
        assert_eq!(report["mutation_enabled"], false);
        assert_unknown_schema_refusal(&report, database, &text_result, &json_result);
    }
}

#[test]
fn empty_schema_doctor_repair_refuses_without_initialization() {
    for empty_sqlite in [false, true] {
        let directory = crate::test_support::temp_home().unwrap();
        let home = directory.path();
        success(home, &["init"]);
        let healthy = diagnosis(home);
        let database = Path::new(healthy["database"].as_str().unwrap());
        fs::write(database, []).unwrap();
        if empty_sqlite {
            let connection = rusqlite::Connection::open(database).unwrap();
            connection
                .execute_batch("CREATE TABLE transient(value TEXT); DROP TABLE transient;")
                .unwrap();
        }
        let before = fs::read(database).unwrap();
        assert_eq!(before.is_empty(), !empty_sqlite);
        let text = run(home, &["doctor", "--repair-projections"]);
        assert!(!text.status.success());
        assert_eq!(fs::read(database).unwrap(), before);
        let json = run(home, &["doctor", "--repair-projections", "--json"]);
        assert!(!json.status.success());
        assert_eq!(fs::read(database).unwrap(), before);
        let report: Value = serde_json::from_slice(&json.stdout).unwrap();
        assert_eq!(report["code"], "store_not_initialized");
        assert_eq!(report["phase"], "projection_repair");
        assert_eq!(report["healthy"], false);
        assert_eq!(report["mutation_enabled"], false);
        assert!(report.get("findings").is_none());
        assert!(report["remedy"].as_str().unwrap().contains("engram init"));
        let text = String::from_utf8(text.stdout).unwrap();
        for (key, value) in report.as_object().unwrap() {
            assert!(text.lines().any(|line| line == format!("{key}: {value}")));
        }
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
        let directory = crate::test_support::temp_home().unwrap();
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
                    report["remedy"],
                    "Use the Engram build that created this store. This build has no in-place store upgrade; projection repair cannot convert a different durable schema."
                );
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
    let directory = crate::test_support::temp_home().unwrap();
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
