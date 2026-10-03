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

// A host selects an evaluator mode the store will admit, so doctor names the
// active evaluation policy in the JSON it reads and in the operator text.
#[test]
fn doctor_reports_the_acceptance_evaluation_policy_in_json_and_text() {
    let directory = crate::test_support::temp_home().unwrap();
    let home = directory.path();
    success(home, &["init"]);
    let self_asserted = diagnosis(home);
    assert_eq!(
        self_asserted["control"]["acceptance_evaluation"]["allowed_modes"],
        json!([])
    );
    let text = String::from_utf8(success(home, &["doctor"]).stdout).unwrap();
    assert!(
        text.contains("Acceptance evaluation: off; completion is self-asserted"),
        "{text}"
    );
    success(
        home,
        &[
            "control-policy",
            "set-acceptance-evaluation",
            "--modes",
            "same-session,independent-session",
            "--mechanical-basis",
            "asserted",
            "--authorized-by",
            "operator",
            "--idempotency-key",
            "doctor-reports-evaluation",
        ],
    );
    let evaluated = diagnosis(home);
    assert_eq!(
        evaluated["control"]["acceptance_evaluation"],
        json!({
            "allowed_modes": ["same_session", "independent_session"],
            "mechanical_basis": "asserted",
            "require_source_freshness": false,
        })
    );
    let text = String::from_utf8(success(home, &["doctor"]).stdout).unwrap();
    assert!(
        text.contains(
            "Acceptance evaluation: required; modes=same_session, independent_session mechanical_basis=asserted source_freshness=not required"
        ),
        "{text}"
    );
    // The per-request read a host uses: the policy head, not the audit.
    let shown: Value =
        serde_json::from_slice(&success(home, &["control-policy", "show"]).stdout).unwrap();
    for key in [
        "policy",
        "epoch",
        "required_assurance",
        "obligation_rules",
        "acceptance_evaluation",
        "supported_effects",
    ] {
        assert_eq!(shown[key], evaluated["control"][key], "{key}");
    }
    assert!(
        shown.get("sessions").is_none(),
        "show prints the policy, not live control counts: {shown}"
    );
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

// The aggregate count covers canonical objects and the projections checked
// against them, so it is labelled as checks; only the verified snapshot line
// counts immutable objects, and the JSON fields keep their names.
#[test]
fn doctor_labels_its_checked_count_apart_from_the_verified_snapshot() {
    let directory = crate::test_support::temp_home().unwrap();
    let home = directory.path();
    success(home, &["init"]);
    let work = ["work", "--actor-id", "labels", "--session-id", "labels"];
    success(home, &[&work[..], &["add", "Counted work"]].concat());
    success(
        home,
        &[
            &work[..],
            &["remember", "A remembered note is projected too"],
        ]
        .concat(),
    );
    let report = diagnosis(home);
    let checked = report["checked"]["objects"].as_u64().unwrap();
    let objects = report["verified_snapshot"]["object_count"]
        .as_u64()
        .unwrap();
    assert_ne!(
        checked, objects,
        "the fixture must make the checks differ from the snapshot's objects"
    );
    let healthy = String::from_utf8(success(home, &["doctor"]).stdout).unwrap();
    let repaired =
        String::from_utf8(success(home, &["doctor", "--repair-projections"]).stdout).unwrap();
    let repaired_json: Value = serde_json::from_slice(
        &success(home, &["doctor", "--repair-projections", "--json"]).stdout,
    )
    .unwrap();
    assert_eq!(repaired_json["checked_objects"], checked);
    for (text, summary) in [
        (&healthy, "Engram store is healthy ("),
        (
            &repaired,
            "Engram rebuildable projections repaired and verified (",
        ),
    ] {
        assert!(
            text.contains(&format!(
                "{summary}{checked} canonical object and projection check(s), "
            )),
            "{text}"
        );
        let immutable: Vec<_> = text
            .lines()
            .filter(|line| line.contains("immutable object"))
            .collect();
        assert_eq!(
            immutable,
            [format!(
                "Verified snapshot: {objects} immutable object(s); selected project feed head position {}",
                report["verified_snapshot"]["project_feed_head"]["position"]
            )],
            "{text}"
        );
    }
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
        "source_revision": env!("ENGRAM_SOURCE_REVISION"),
    });
    let fingerprint = CanonicalObject::freeze(&build).unwrap().key().clone();
    assert_eq!(doctor["build"], build);
    assert_eq!(doctor["build_fingerprint"], json!(fingerprint));
    assert_eq!(diagnosis(home)["build_fingerprint"], json!(fingerprint));
    let version = success(home, &["--version"]);
    assert_eq!(version.stdout, success(home, &["-V"]).stdout);
    assert_eq!(
        String::from_utf8(version.stdout).unwrap().trim(),
        format!(
            "engram {} build {} (exe {}, schema {}, rev {})",
            env!("CARGO_PKG_VERSION"),
            &fingerprint.as_str()[..12],
            &build["executable_sha256"].as_str().unwrap()[..12],
            &build["schema_reference"].as_str().unwrap()[..12],
            engram::build_identity::short_revision(env!("ENGRAM_SOURCE_REVISION")),
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
            let observed = report["findings"].as_array().unwrap();
            assert!(!observed.is_empty(), "{observed:?}");
        }
        let text = String::from_utf8(text_result.stdout).unwrap();
        assert_refusal_text_mirrors_report(&report, &text);
    }
}

/// A text refusal prints each field of the JSON report as `key: value`,
/// except the backup block, which it prints as `backup status` does.
fn assert_refusal_text_mirrors_report(report: &Value, text: &str) {
    for (key, value) in report.as_object().unwrap() {
        if key == "backup" {
            let opening = if value.get("unavailable").is_some() {
                "backup: not read ("
            } else {
                "backup mode: "
            };
            assert!(
                text.lines().any(|line| line.starts_with(opening)),
                "missing the backup block: {text}"
            );
            continue;
        }
        assert!(
            text.lines().any(|line| line == format!("{key}: {value}")),
            "missing {key}"
        );
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
            _ => {
                let listed_findings = report["findings"].as_array().unwrap();
                assert!(!listed_findings.is_empty(), "{listed_findings:?}");
            }
        }
        let refused_text = run(home, &["doctor"]);
        assert!(!refused_text.status.success());
        let text = String::from_utf8(refused_text.stdout).unwrap();
        assert_refusal_text_mirrors_report(&report, &text);
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

/// Runs read-only policy recovery on `home` and returns its JSON report and
/// text output, checking that the store's bytes did not change.
fn recovery_reports(home: &Path, database: &Path) -> (Value, String) {
    let before = fs::read(database).unwrap();
    let json = run(home, &["doctor", "--recover-policy", "--json"]);
    let text = run(home, &["doctor", "--recover-policy"]);
    assert_eq!(fs::read(database).unwrap(), before, "recovery is read-only");
    let mut text_output = String::from_utf8(text.stdout).unwrap();
    text_output.push_str(&String::from_utf8(text.stderr).unwrap());
    let report: Value = serde_json::from_slice(&json.stdout).unwrap_or_else(|_| {
        panic!(
            "recovery JSON: {}{}",
            String::from_utf8_lossy(&json.stdout),
            String::from_utf8_lossy(&json.stderr)
        )
    });
    (report, text_output)
}

// A recovery finding names the record and the shape of the problem: an
// unknown or missing member by name, otherwise the category and position,
// never a value the record holds.
#[test]
fn policy_recovery_never_repeats_a_value_from_a_record_that_does_not_decode() {
    let directory = crate::test_support::temp_home().unwrap();
    let home = directory.path();
    success(home, &["init"]);
    let database = Path::new(diagnosis(home)["database"].as_str().unwrap()).to_owned();
    let (healthy, healthy_text) = recovery_reports(home, &database);
    assert_eq!(
        healthy["control_policy"]["invalid_control_records"],
        json!([])
    );
    assert!(!healthy_text.contains("INVALID"), "{healthy_text}");

    let connection = rusqlite::Connection::open(&database).unwrap();
    let (rule_set, stored): (String, Vec<u8>) = connection
        .query_row(
            "SELECT object_id, canonical_json FROM objects WHERE object_kind = 'obligation_rule_set'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let stored = String::from_utf8(stored).unwrap();
    let marker = "\"check_kind\":\"";
    let start = stored
        .find(marker)
        .expect("the stock rule set names a check kind")
        + marker.len();
    let end = start + stored[start..].find('"').unwrap();
    let hostile = "hostile-check-kind-7f3a";
    let store_rule_set = |bytes: &str| {
        connection
            .execute(
                "UPDATE objects SET canonical_json = ?1 WHERE object_id = ?2",
                rusqlite::params![bytes.as_bytes(), rule_set],
            )
            .unwrap();
    };
    // A member name holding an escape sequence, written as JSON escapes it,
    // beside the check kind in the requirement that refuses unknown members.
    let member_at = start - marker.len();
    let unknown_member = format!(
        "{}\"evil\\u001b[31m\":1,{}",
        &stored[..member_at],
        &stored[member_at..]
    );
    for (bytes, shape, hidden) in [
        (
            format!("{}{hostile}{}", &stored[..start], &stored[end..]),
            "a field of the wrong type or value at line 1 column",
            Some(hostile),
        ),
        (unknown_member, "unknown field `evil\u{1b}[31m`", None),
        (
            stored.replacen("\"rules\":", "\"rules_renamed\":", 1),
            "missing field `rules`",
            None,
        ),
        (
            stored[..stored.len() - 2].to_owned(),
            "truncated JSON at line 1 column",
            None,
        ),
        (
            format!("{}#{}", &stored[..member_at], &stored[member_at..]),
            "malformed JSON at line 1 column",
            None,
        ),
    ] {
        store_rule_set(&bytes);
        let (report, text) = recovery_reports(home, &database);
        let findings = report["control_policy"]["invalid_control_records"]
            .as_array()
            .unwrap();
        assert!(!findings.is_empty(), "{report}");
        let details: Vec<&str> = findings
            .iter()
            .map(|finding| finding["detail"].as_str().unwrap())
            .collect();
        assert!(
            details
                .iter()
                .any(|detail| detail.starts_with("a record does not decode: ")),
            "{details:?}"
        );
        if let Some(hidden) = hidden {
            assert!(!report.to_string().contains(hidden), "{report}");
            assert!(!text.contains(hidden), "{text}");
            assert!(
                details.iter().any(|detail| detail.contains(shape)),
                "{details:?}"
            );
            assert!(text.contains(shape), "{text}");
        } else if shape.starts_with("unknown field `evil") {
            // The member is named, and the text framing escapes its control.
            assert!(
                details
                    .iter()
                    .any(|detail| detail.contains("unknown field `evil\u{1b}[31m`")),
                "{details:?}"
            );
            assert!(!text.contains('\u{1b}'), "{text}");
            assert!(text.contains("unknown field `evil"), "{text}");
        } else {
            assert!(
                details.iter().any(|detail| detail.contains(shape)),
                "{details:?}"
            );
            assert!(text.contains(shape), "{text}");
        }
        assert!(text.contains("INVALID control_policy"), "{text}");
    }
    store_rule_set(&stored);
    let (restored, _) = recovery_reports(home, &database);
    assert_eq!(
        restored["control_policy"]["invalid_control_records"],
        json!([])
    );

    // A stored record id column holding text that is no id is named by its
    // shape too, never repeated.
    let authority: String = connection
        .query_row(
            "SELECT authority_id FROM control_policy_versions",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let hostile_id = "hostile-authority-id-9c1e";
    // The corruption fixture writes around the foreign key a writer keeps.
    connection
        .execute_batch("PRAGMA foreign_keys = OFF")
        .unwrap();
    connection
        .execute(
            "UPDATE control_policy_versions SET authority_id = ?1",
            [hostile_id],
        )
        .unwrap();
    let (report, text) = recovery_reports(home, &database);
    assert!(!report.to_string().contains(hostile_id), "{report}");
    assert!(!text.contains(hostile_id), "{text}");
    assert!(
        report["control_policy"]["invalid_control_records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["detail"] == "a stored record id is not a valid record id"),
        "{report}"
    );
    connection
        .execute(
            "UPDATE control_policy_versions SET authority_id = ?1",
            [&authority],
        )
        .unwrap();
    let (restored, _) = recovery_reports(home, &database);
    assert_eq!(
        restored["control_policy"]["invalid_control_records"],
        json!([])
    );

    // A canonical object stored under another kind is named by the kind that
    // was asked for, never by the stored kind text.
    let hostile_kind = "hostile-kind-5d2b";
    connection
        .execute(
            "UPDATE objects SET object_kind = ?1 WHERE object_id = ?2",
            rusqlite::params![hostile_kind, rule_set],
        )
        .unwrap();
    let (report, text) = recovery_reports(home, &database);
    assert!(!report.to_string().contains(hostile_kind), "{report}");
    assert!(!text.contains(hostile_kind), "{text}");
    assert!(
        report["control_policy"]["invalid_control_records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["detail"].as_str().unwrap().ends_with(
                "is stored under another kind than the \"obligation_rule_set\" requested"
            )),
        "{report}"
    );
    connection
        .execute(
            "UPDATE objects SET object_kind = 'obligation_rule_set' WHERE object_id = ?1",
            [&rule_set],
        )
        .unwrap();
    let (restored, _) = recovery_reports(home, &database);
    assert_eq!(
        restored["control_policy"]["invalid_control_records"],
        json!([])
    );
}
