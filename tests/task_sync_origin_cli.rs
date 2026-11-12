#[path = "common/task_sync_opt_in.rs"]
mod task_sync_opt_in;
#[path = "../src/test_support.rs"]
mod test_support;

use engram::{ProjectId, project_database_path};
use serde_json::Value;
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn command(home: &Path, marker: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_engram"));
    command
        .args(["--home"])
        .arg(home)
        .arg("--project-file")
        .arg(marker)
        .env("ENGRAM_ACTOR_ID", "origin-cli-operator")
        .env("ENGRAM_SESSION_ID", "origin-cli-session")
        .env_remove("ENGRAM_ACTOR_CONTEXT");
    command
}

fn value(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn enroll(home: &Path, marker: &Path, operation: uuid::Uuid, predecessor: Option<&str>) -> Output {
    let mut command = command(home, marker);
    command.args([
        "task-sync",
        "origin-enroll",
        "--operation",
        &operation.to_string(),
    ]);
    if let Some(predecessor) = predecessor {
        command.args(["--predecessor", predecessor]);
    }
    command.output().unwrap()
}

#[test]
fn origin_cli_recovers_a_missing_witness_with_an_explicit_successor_and_exact_receipt() {
    let owned = test_support::temp_home().unwrap();
    let marker = owned.path().join(".engram-project");
    fs::write(&marker, "origin-recovery-cli\n").unwrap();
    let home = owned.path().join("home");
    let init = command(&home, &marker).arg("init").output().unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    task_sync_opt_in::enable(&home, "origin-recovery-cli");
    let original = value(&enroll(&home, &marker, uuid::Uuid::new_v4(), None));
    let predecessor = original["origin_id"].as_str().unwrap();
    let database = project_database_path(&home, &ProjectId("origin-recovery-cli".into()));
    let witness = database
        .parent()
        .unwrap()
        .join("task-sync-origin-registration.json");
    fs::remove_file(&witness).unwrap();
    assert!(
        !enroll(&home, &marker, uuid::Uuid::new_v4(), Some(predecessor))
            .status
            .success()
    );
    let operation = uuid::Uuid::new_v4().to_string();
    let recover = || {
        command(&home, &marker)
            .args([
                "task-sync",
                "origin-recover",
                "--operation",
                &operation,
                "--predecessor",
                predecessor,
                "--cause",
                "migration-import",
            ])
            .output()
            .unwrap()
    };
    let receipt = value(&recover());
    assert_ne!(receipt["record"]["successor_origin"], original["origin_id"]);
    assert_eq!(
        receipt["record"]["request"]["predecessor"],
        original["origin_id"]
    );
    assert_eq!(receipt["record"]["request"]["cause"], "migration_import");
    assert!(witness.exists());
    assert_eq!(value(&recover()), receipt);
    assert_eq!(canonical_counts(&database), (2, 2));
}

fn canonical_counts(database: &Path) -> (i64, i64) {
    let connection = rusqlite::Connection::open(database).unwrap();
    (
        connection.query_row("SELECT count(*) FROM objects WHERE object_kind='task_sync_producing_origin'", [], |row|row.get(0)).unwrap(),
        connection.query_row("SELECT count(*) FROM work_feed_entries WHERE object_kind='task_sync_producing_origin'", [], |row|row.get(0)).unwrap(),
    )
}

#[test]
fn origin_cli_opt_in_enrollment_retry_fork_and_peek_delivery() {
    let owned = test_support::temp_home().unwrap();
    let marker = owned.path().join(".engram-project");
    fs::write(&marker, "origin-cli\n").unwrap();
    let home = owned.path().join("home");
    let init = command(&home, &marker).arg("init").output().unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let database = project_database_path(&home, &ProjectId("origin-cli".into()));
    assert_eq!(
        value(
            &command(&home, &marker)
                .args(["task-sync", "origin"])
                .output()
                .unwrap()
        ),
        Value::Null
    );
    let operation = uuid::Uuid::new_v4();
    let before = fs::read(&database).unwrap();
    let refused = enroll(&home, &marker, operation, None);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("task sync is not enabled"));
    assert_eq!(fs::read(&database).unwrap(), before);
    assert!(
        !database
            .parent()
            .unwrap()
            .join("task-sync-origin-registration.json")
            .exists()
    );

    task_sync_opt_in::enable(&home, "origin-cli");
    let first = value(&enroll(&home, &marker, operation, None));
    assert_eq!(first["enrollment"]["project_id"], "origin-cli");
    assert_eq!(
        first["enrollment"]["enrollment_operation_id"],
        operation.to_string()
    );
    assert!(first["enrollment"]["predecessor_origin_id"].is_null());
    assert_eq!(canonical_counts(&database), (1, 1));
    assert_eq!(value(&enroll(&home, &marker, operation, None)), first);
    assert_eq!(canonical_counts(&database), (1, 1));
    assert_eq!(
        value(
            &command(&home, &marker)
                .args(["task-sync", "origin"])
                .output()
                .unwrap()
        ),
        first
    );

    let second_operation = uuid::Uuid::new_v4();
    let second = value(&enroll(
        &home,
        &marker,
        second_operation,
        first["origin_id"].as_str(),
    ));
    assert_ne!(second["origin_id"], first["origin_id"]);
    assert_eq!(
        second["enrollment"]["predecessor_origin_id"],
        first["origin_id"]
    );
    assert_eq!(canonical_counts(&database), (2, 2));
    let invalid = command(&home, &marker)
        .args(["task-sync", "origin-enroll", "--operation", "not-a-uuid"])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert_eq!(canonical_counts(&database), (2, 2));

    let cancel_operation = uuid::Uuid::new_v4();
    let pending_path = database
        .parent()
        .unwrap()
        .join("task-sync-origin-pending.json");
    let witness_path = database
        .parent()
        .unwrap()
        .join("task-sync-origin-registration.json");
    let witness_bytes = fs::read(&witness_path).unwrap();
    let pending = serde_json::json!({
        "format": "engram-task-sync-origin-pending-v1",
        "origin": engram::ObjectId::mint(),
        "record": {
            "project_id": "origin-cli", "predecessor_origin_id": second["origin_id"],
            "enrollment_operation_id": cancel_operation, "actor": second["enrollment"]["actor"],
            "enrolled_at": chrono::Utc::now(),
        },
        "expected_witness": serde_json::from_slice::<Value>(&witness_bytes).unwrap(),
    });
    fs::write(&pending_path, serde_json::to_vec(&pending).unwrap()).unwrap();
    let disabled = command(&home, &marker)
        .args(["task-sync", "disable", "--json"])
        .output()
        .unwrap();
    assert!(disabled.status.success());
    let before_cancel = fs::read(&database).unwrap();
    let pending_bytes = fs::read(&pending_path).unwrap();
    let wrong_predecessor = command(&home, &marker)
        .args([
            "task-sync",
            "origin-cancel",
            "--operation",
            &cancel_operation.to_string(),
            "--predecessor",
            first["origin_id"].as_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!wrong_predecessor.status.success());
    assert_eq!(fs::read(&pending_path).unwrap(), pending_bytes);
    assert_eq!(fs::read(&database).unwrap(), before_cancel);
    for expected in [true, false] {
        let result = value(
            &command(&home, &marker)
                .args([
                    "task-sync",
                    "origin-cancel",
                    "--operation",
                    &cancel_operation.to_string(),
                    "--predecessor",
                    second["origin_id"].as_str().unwrap(),
                ])
                .output()
                .unwrap(),
        );
        assert_eq!(result["cancelled"], expected);
        assert!(!pending_path.exists());
        assert_eq!(fs::read(&witness_path).unwrap(), witness_bytes);
        assert_eq!(fs::read(&database).unwrap(), before_cancel);
        assert_eq!(canonical_counts(&database), (2, 2));
    }

    let before = fs::read(&database).unwrap();
    let mut peek = command(&home, &marker);
    peek.env("ENGRAM_SESSION_ID", "origin-cli-observer").args([
        "work",
        "next",
        "--peek",
        "--verbose",
        "--json",
    ]);
    let next = value(&peek.output().unwrap());
    let encoded = serde_json::to_string(&next).unwrap();
    assert!(encoded.contains("task_sync_origin_enrolled"), "{encoded}");
    assert!(
        encoded.contains(second["origin_id"].as_str().unwrap()),
        "{encoded}"
    );
    assert_eq!(fs::read(&database).unwrap(), before, "peek is read-only");
}
