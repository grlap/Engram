#[path = "../src/test_support.rs"]
mod test_support;

use std::process::Command;

#[cfg(windows)]
#[test]
fn migration_import_names_long_staging_paths_without_creating_anything() {
    use std::{fs, os::windows::ffi::OsStrExt, path::Path};

    let directory = test_support::temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    drop(engram::SqliteStore::open_unresolved(&source).expect("store"));
    let file = directory.path().join("export.jsonl");
    engram::storage::migration::export_json(&source, &file).expect("valid export");
    let root = std::path::absolute(directory.path()).expect("absolute fixture");
    let root_units = root.as_os_str().encode_wide().count();
    assert!(
        root_units < 219,
        "fixture root too long: {}",
        root.display()
    );
    let deep = root.join("a".repeat(220 - root_units - 1));
    fs::create_dir(&deep).expect("owned, creatable parent");
    let target = deep.join("out.db");
    let staged_wal = deep.join(format!(".engram-migration-{}.tmp-wal", uuid::Uuid::nil()));
    assert!(staged_wal.as_os_str().encode_wide().count() > 260);
    assert!(target.as_os_str().encode_wide().count() + "-journal".len() < 260);
    let listing = |path: &Path| {
        let mut names = fs::read_dir(path)
            .expect("parent listing")
            .map(|entry| entry.expect("entry").file_name())
            .collect::<Vec<_>>();
        names.sort();
        names
    };
    let before = listing(&deep);
    let run = |out: &Path| {
        Command::new(env!("CARGO_BIN_EXE_engram"))
            .current_dir(&root)
            .args(["migration", "import", "--file"])
            .arg(&file)
            .arg("--out")
            .arg(out)
            .output()
            .expect("CLI")
    };
    let output = run(&target);
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{error}");
    assert!(error.contains("Windows import path"), "{error}");
    assert!(error.contains(".engram-migration-"), "{error}");
    let staged_units = staged_wal.as_os_str().encode_wide().count() - "-wal".len();
    assert!(
        error.contains(&format!("length {staged_units} UTF-16 code units")),
        "{error}"
    );
    assert!(
        error.contains("260") && error.contains("shorter output path"),
        "{error}"
    );
    assert!(!target.exists());
    assert_eq!(
        listing(&deep),
        before,
        "no file, directory or sidecar created"
    );

    // The same export is admitted at a short output path.
    let short = root.join("short.db");
    let output = run(&short);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        engram::SqliteStore::open_unresolved(&short)
            .expect("short import")
            .verify_all()
            .expect("doctor")
            .is_healthy()
    );

    let help = Command::new(env!("CARGO_BIN_EXE_engram"))
        .args(["migration", "import", "--help"])
        .output()
        .expect("help");
    assert!(help.status.success());
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(
        help.contains("260") && help.contains("UTF-16") && help.contains("short output directory")
    );
}

#[test]
fn migration_cli_exports_and_imports_explicit_files_without_selecting_a_project_or_active_home() {
    let directory = test_support::temp_home().expect("fixture");
    // Explicit-file migration bypasses project selection, even beside an invalid marker.
    std::fs::write(directory.path().join(".engram-project"), b"\xff").unwrap();
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
        assert_eq!(
            std::fs::read(directory.path().join(".engram-project")).unwrap(),
            b"\xff"
        );
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
