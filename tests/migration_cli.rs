#[path = "../src/test_support.rs"]
mod test_support;

use std::process::Command;

#[test]
fn migration_cli_exports_and_imports_explicit_files_without_project_or_active_home() {
    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    drop(engram::SqliteStore::open_unresolved(&source).expect("current store"));
    let file = directory.path().join("export.jsonl");
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
        assert!(!absent_home.exists());
        assert!(!directory.path().join(".engram-project").exists());
        output
    };
    let exported = run(
        "export",
        &[
            "--database",
            source.to_str().unwrap(),
            "--out",
            file.to_str().unwrap(),
        ],
    );
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&exported.stdout).expect("report");
    assert!(report["rows"].as_u64().expect("rows") > 0);
    assert!(
        String::from_utf8_lossy(&exported.stderr).contains("private"),
        "the export warns that it holds private data"
    );

    let import = [
        "--file",
        file.to_str().unwrap(),
        "--out",
        imported.to_str().unwrap(),
    ];
    let first = run("import", &import);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&first.stdout).expect("report");
    assert!(
        report["tables"]
            .as_array()
            .is_some_and(|tables| !tables.is_empty())
    );
    let store = engram::SqliteStore::open_unresolved(&imported).expect("imported store opens");
    assert!(store.verify_all().expect("doctor").is_healthy());
    drop(store);

    // Neither word ever replaces a file that is already there.
    let before = std::fs::read(&imported).expect("imported bytes");
    assert!(!run("import", &import).status.success());
    assert_eq!(std::fs::read(&imported).expect("imported bytes"), before);
    assert!(
        !run(
            "export",
            &[
                "--database",
                source.to_str().unwrap(),
                "--out",
                file.to_str().unwrap(),
            ],
        )
        .status
        .success()
    );
}
