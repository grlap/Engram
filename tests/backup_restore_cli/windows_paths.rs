use super::*;

fn utf16_length(path: &Path) -> usize {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().count()
}

fn staging_path(homes: &Homes, home: &str) -> PathBuf {
    homes
        .database(home)
        .parent()
        .unwrap()
        .join(format!(".backup-restore-{}.staging", uuid::Uuid::now_v7()))
}

/// Pad only runtime data, using short individual components. A deeper
/// launcher root still exercises the long-path regression rather than skipping it.
fn home_for_staging_length(homes: &Homes, label: &str, desired: usize) -> String {
    let mut home = label.to_owned();
    while utf16_length(&staging_path(homes, &home)) < desired {
        let remaining = desired - utf16_length(&staging_path(homes, &home));
        if remaining <= 60 {
            home.push_str(&"x".repeat(remaining));
        } else {
            home.push('/');
            home.push_str(&"x".repeat(59));
        }
    }
    home
}

#[test]
fn long_archive_paths_abandon_pending_without_contacting_the_target() {
    use engram::backup::{CopyKind, target::RecordPaths};

    for desired in [260, 400] {
        let homes = Homes::new();
        let mut name = "zażółć 🦀".to_owned();
        let archive_path = |name: &str| {
            RecordPaths::new(
                &homes.path(name),
                &ProjectId(PROJECT.into()),
                CopyKind::Store,
            )
            .directory
            .join("store.restore-abandoned-20261002T000000Z.json")
        };
        let minimum = utf16_length(&archive_path(&name));
        while utf16_length(&archive_path(&name)) < desired {
            let remaining = desired - utf16_length(&archive_path(&name));
            if remaining <= 60 {
                name.push_str(&"x".repeat(remaining));
            } else {
                name.push('/');
                name.push_str(&"x".repeat(59));
            }
        }
        let archive = abandon_pending_without_contacting_target(&homes, &name);
        assert!(utf16_length(&archive) >= desired);
        if minimum <= desired {
            assert_eq!(utf16_length(&archive), desired);
        }
        assert!(!archive.as_os_str().to_string_lossy().starts_with(r"\\?\"));
    }
}

#[test]
fn long_staging_and_store_paths_restore_reopen_and_capture() {
    let homes = Homes::new();
    let manifest = homes.origin_copy();
    let copy = manifest["copy"].as_str().unwrap();
    for (label, desired) in [("boundary", 260), ("beyond", 400)] {
        let minimum = utf16_length(&staging_path(&homes, label));
        let home = home_for_staging_length(&homes, label, desired);
        let staging = staging_path(&homes, &home);
        assert!(utf16_length(&staging) >= 260);
        if minimum <= desired {
            assert_eq!(utf16_length(&staging), desired);
        }
        let database = homes.database(&home);
        if desired > 260 {
            assert!(utf16_length(&database) > 260);
        }
        homes.set_target(&home);
        let restored: Value = serde_json::from_slice(
            &homes
                .succeeded(
                    &home,
                    &[
                        "backup",
                        "restore",
                        copy,
                        "--origin-retired-by",
                        "greg",
                        "--json",
                    ],
                )
                .stdout,
        )
        .unwrap();
        assert_eq!(restored["store"], database.display().to_string());
        assert_eq!(restored["sha256"], manifest["capture"]["sha256"]);
        assert_eq!(
            sha256(&fs::read(&database).unwrap()),
            manifest["capture"]["sha256"]
        );
        assert_eq!(homes.store_directory_names(&home), ["engram.db"]);
        let readiness: Value =
            serde_json::from_slice(&homes.succeeded(&home, &["readiness", "--json"]).stdout)
                .unwrap();
        assert_eq!(readiness["ready"], true, "{readiness}");
        homes.succeeded(&home, &["backup", "push"]);
        assert!(homes.newest_manifest(&home)["copy"].is_string());
        // The immutable backup verifier and ordinary writable opener both
        // address the same original logical path, including URI metacharacters.
        let out = database.parent().unwrap().join("space % # zażółć 🦀.db");
        let store = engram::SqliteStore::open(&database).unwrap();
        let backup = store.backup_to(&out).unwrap();
        assert_eq!(backup.path, out);
        assert_eq!(
            engram::SqliteStore::verify_backup(&out).unwrap().file_bytes,
            backup.file_bytes
        );
    }
}
