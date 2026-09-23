//! The reserved file, its privacy, and publication: the store is built in
//! place, stays owner-only, and is never published beside a sidecar.

use super::*;

#[test]
fn the_store_is_built_in_the_reserved_file_itself() {
    use std::io::Read;

    let directory = crate::test_support::temp_home().expect("directory");
    let staged = Staged::beside(&directory.path().join("out.db")).expect("reserve");
    // A handle on the file that was reserved. If the initializer deleted that
    // file and let SQLite create a replacement — the sequence that widens the
    // permissions — this handle would still refer to the reserved file and
    // would never see a database header through it.
    let mut reserved = fs::File::open(&staged.path).expect("hold the reserved file open");

    let store = SqliteStore::open_unresolved(&staged.path).expect("open the reserved file");
    // The mode is inspected here, before any sensitive row is written, not only
    // on the published result.
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let reserved_identity = reserved.metadata().expect("metadata").ino();
        let opened = fs::metadata(&staged.path).expect("metadata");
        assert_eq!(
            opened.ino(),
            reserved_identity,
            "the same file, not a new one"
        );
        assert_eq!(
            opened.permissions().mode() & 0o777,
            0o600,
            "private before the first sensitive write"
        );
    }
    assert!(store.verify_all().expect("doctor").is_healthy());
    drop(store);

    let mut header = [0_u8; 16];
    reserved
        .read_exact(&mut header)
        .expect("the reserved file now holds the store");
    assert_eq!(
        &header, b"SQLite format 3\0",
        "the store was built in the reserved file"
    );

    // What this shows and does not show: the initializer import uses opens the
    // reserved file in place, so the mode it was created with still governs
    // every later write. The journals import itself writes are not observed
    // here; their privacy rests on SQLite matching the database file's mode,
    // which the unix test below exercises on a store of the same mode.
}

#[cfg(unix)]
#[test]
fn a_journal_beside_a_private_store_is_private_too() {
    use std::os::unix::fs::PermissionsExt;

    let directory = crate::test_support::temp_home().expect("directory");
    let staged = Staged::beside(&directory.path().join("out.db")).expect("reserve");
    let store = SqliteStore::open_unresolved(&staged.path).expect("open the reserved file");
    // Hold a write open, so a journal exists while it is inspected.
    store
        .connection
        .execute_batch("BEGIN IMMEDIATE; CREATE TABLE probe (value TEXT);")
        .expect("begin a write");
    let mut found = 0;
    for suffix in ["-wal", "-journal"] {
        let sidecar = PathBuf::from(format!("{}{suffix}", staged.path.display()));
        if sidecar.exists() {
            found += 1;
            assert_eq!(
                fs::metadata(&sidecar)
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600,
                "{suffix} carries the same rows as the store"
            );
        }
    }
    assert!(found > 0, "the fixture needs an active journal");
    store
        .connection
        .execute_batch("ROLLBACK;")
        .expect("release the write");
}

// Windows has no POSIX mode, so this pins the permission itself only where the
// permission exists.
#[cfg(unix)]
#[test]
fn an_imported_store_and_its_journals_stay_private() {
    use std::os::unix::fs::PermissionsExt;

    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    populated(&source);
    let file = directory.path().join("export.jsonl");
    export_json(&source, &file).expect("export");
    let target = directory.path().join("target.db");
    import_json(&file, &target).expect("import");
    let mode = |path: &Path| fs::metadata(path).expect("metadata").permissions().mode() & 0o777;
    assert_eq!(
        mode(&target),
        0o600,
        "the imported store holds private data"
    );
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = PathBuf::from(format!("{}{suffix}", target.display()));
        if sidecar.exists() {
            assert_eq!(mode(&sidecar), 0o600, "{suffix} carries the same rows");
        }
    }
}

#[test]
fn a_failed_publication_names_the_operation_and_keeps_the_cause() {
    use std::io::{Error, ErrorKind};

    let destination = Path::new("C:/somewhere/new-store.db");
    let mapped = |kind: ErrorKind| publish_failure(destination, &Error::new(kind, "the cause"));
    assert!(matches!(
        mapped(ErrorKind::AlreadyExists),
        MigrationError::Refused(reason) if reason == "destination already exists"
    ));
    for kind in [ErrorKind::Unsupported, ErrorKind::PermissionDenied] {
        assert!(
            matches!(mapped(kind), MigrationError::Refused(reason)
                if reason.contains("hard link") && reason.contains("new-store.db") && reason.contains("the cause")),
            "{kind:?}"
        );
    }
    // Any other failure keeps its kind and its cause, and says what was
    // being done, instead of arriving as a bare I/O error.
    for kind in [
        ErrorKind::Other,
        ErrorKind::NotFound,
        ErrorKind::Interrupted,
    ] {
        let MigrationError::Io(error) = mapped(kind) else {
            panic!("{kind:?} is an I/O failure");
        };
        assert_eq!(error.kind(), kind);
        let text = error.to_string();
        assert!(
            text.contains("publishing") && text.contains("new-store.db"),
            "{text}"
        );
        assert!(text.contains("the cause"), "{text}");
    }
}

#[test]
fn a_sidecar_beside_the_destination_refuses_the_import() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    populated(&source);
    let file = directory.path().join("export.jsonl");
    export_json(&source, &file).expect("export");
    let target = directory.path().join("target.db");
    // A log or journal left at the destination's name belongs to another
    // database, and SQLite would apply it to the imported one. Each is refused
    // by name before anything is staged.
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut name = target.as_os_str().to_os_string();
        name.push(suffix);
        let sidecar = std::path::PathBuf::from(name);
        fs::write(&sidecar, b"left behind by the old database").expect("sidecar");
        let error = import_json(&file, &target).expect_err(suffix);
        assert!(
            matches!(&error, MigrationError::Refused(reason)
                if reason.contains(suffix) && reason.contains("beside the destination")),
            "{suffix}: {error}"
        );
        assert!(!target.exists(), "{suffix}: a store was published");
        assert_eq!(
            staging_leftovers(directory.path()),
            0,
            "{suffix}: a staging file was left"
        );
        fs::remove_file(&sidecar).expect("remove the sidecar");
    }
    import_json(&file, &target).expect("with nothing beside it, the import publishes");
}
