#[path = "../src/test_support.rs"]
mod test_support;

use std::process::Command;

use engram::storage::migration::ExportManifest;

#[test]
fn migration_cli_uses_explicit_files_without_project_or_active_home() {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("old.db");
    let archive = directory.path().join("export.db");
    let absent_home = directory.path().join("must-not-create");
    let connection = rusqlite::Connection::open(&source).expect("source");
    connection
        .execute_batch("CREATE TABLE older_format(x); INSERT INTO older_format VALUES (x'00ff');")
        .expect("fixture");
    drop(connection);
    for operation in ["export", "verify", "compare"] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_engram"));
        command
            .current_dir(directory.path())
            .arg("--home")
            .arg(&absent_home)
            .arg("migration")
            .arg(operation);
        if operation != "verify" {
            command.arg("--database").arg(&source);
        }
        command
            .arg(if operation == "export" {
                "--out"
            } else {
                "--archive"
            })
            .arg(&archive);
        let output = command.output().expect("CLI");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let manifest: ExportManifest = serde_json::from_slice(&output.stdout).expect("manifest");
        assert_eq!(manifest.total_rows, 1);
        assert_eq!(manifest.tables[0].name, "older_format");
        assert!(!absent_home.exists());
        assert!(!directory.path().join(".engram-project").exists());
    }
}

#[test]
fn migration_cli_imports_current_archive_without_project_home() {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("current.db");
    drop(engram::SqliteStore::open_unresolved(&source).expect("current store"));
    let archive = directory.path().join("export.db");
    let imported = directory.path().join("imported.db");
    let absent_home = directory.path().join("must-not-create");
    let run = |operation: &str, extra: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_engram"))
            .current_dir(directory.path())
            .arg("--home")
            .arg(&absent_home)
            .arg("migration")
            .arg(operation)
            .args(extra)
            .output()
            .expect("CLI");
        assert!(
            output.status.success(),
            "{operation}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!absent_home.exists());
    };
    run(
        "export",
        &[
            "--database",
            source.to_str().unwrap(),
            "--out",
            archive.to_str().unwrap(),
        ],
    );
    run(
        "import",
        &[
            "--archive",
            archive.to_str().unwrap(),
            "--out",
            imported.to_str().unwrap(),
        ],
    );
    let store = engram::SqliteStore::open_unresolved(&imported).expect("imported current");
    assert!(store.verify_all().expect("doctor").is_healthy());
    assert!(!directory.path().join(".engram-project").exists());
}
