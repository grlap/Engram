use super::*;

#[test]
fn immutable_filename_round_trips_uri_bytes_and_reads_only_the_settled_copy() {
    let directory = crate::test_support::temp_home().unwrap();
    let live = directory.path().join("live.db");
    #[cfg(not(windows))]
    let copy = directory.path().join("space % # ? zażółć 🦀.db");
    // '?' is not a legal Windows filename; cover that byte in the structural test.
    #[cfg(windows)]
    let copy = directory.path().join("space % # zażółć 🦀.db");
    let writer = open_sqlite_file(&live, OpenFlags::default()).unwrap();
    writer.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE proof(value TEXT); INSERT INTO proof VALUES ('settled bytes');").unwrap();
    vacuum_into(&writer, &copy).unwrap();
    drop(writer);
    let before = std::fs::read(&copy).unwrap();
    let immutable = open_immutable(&copy).unwrap();
    assert_eq!(
        immutable
            .query_row("SELECT value FROM proof", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "settled bytes"
    );
    assert!(
        immutable
            .execute("INSERT INTO proof VALUES ('forbidden')", [])
            .is_err()
    );
    drop(immutable);
    assert_eq!(std::fs::read(&copy).unwrap(), before);
    for sidecar in super::super::store_sidecars(&copy) {
        assert!(!sidecar.exists(), "{}", sidecar.display());
    }
}

#[test]
fn ordinary_file_connections_keep_wal_and_writer_locking() {
    let directory = crate::test_support::temp_home().unwrap();
    let path = directory.path().join("locked.db");
    let first = open_sqlite_file(&path, OpenFlags::default()).unwrap();
    first.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE proof(value INTEGER); BEGIN IMMEDIATE; INSERT INTO proof VALUES (7);").unwrap();
    let second = open_sqlite_file(&path, OpenFlags::default()).unwrap();
    second.busy_timeout(std::time::Duration::ZERO).unwrap();
    assert_eq!(
        second
            .execute_batch("BEGIN IMMEDIATE")
            .unwrap_err()
            .sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy)
    );
    assert_eq!(
        second
            .query_row("SELECT count(*) FROM proof", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        0
    );
    first.execute_batch("COMMIT").unwrap();
    assert_eq!(
        second
            .query_row("SELECT value FROM proof", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        7
    );
}

#[cfg(windows)]
#[test]
fn native_paths_and_immutable_uri_preserve_drive_unc_and_unicode() {
    let ordinary = Path::new(r"C:\folder\..\space % # ? zażółć 🦀.db");
    let native = sqlite_filename(ordinary).unwrap();
    assert_eq!(native, r"\\?\C:\space % # ? zażółć 🦀.db");
    assert_eq!(sqlite_filename(Path::new(&native)).unwrap(), native);
    assert_eq!(
        sqlite_filename(Path::new(r"\\server\share\folder\copy.db")).unwrap(),
        r"\\?\UNC\server\share\folder\copy.db"
    );
    let uri = immutable_uri(ordinary).unwrap();
    assert!(uri.starts_with("file:/%5C%5C%3F%5CC:"), "{uri}");
    assert!(uri.contains("%20%25%20%23%20%3F%20"), "{uri}");
    assert!(uri.contains("%F0%9F%A6%80"), "{uri}");
    assert!(uri.ends_with("?immutable=1"), "{uri}");
    assert_eq!(immutable_uri(Path::new(&native)).unwrap(), uri);
}

#[cfg(windows)]
#[test]
fn invalid_unicode_and_excess_capacity_refuse_without_lossy_addressing() {
    use std::os::windows::ffi::OsStringExt;
    let bad = std::ffi::OsString::from_wide(&[
        u16::from(b'C'),
        u16::from(b':'),
        u16::from(b'\\'),
        0xd800,
    ]);
    assert!(matches!(
        validate_sqlite_file_path(Path::new(&bad)),
        Err(StoreError::SqlitePath { .. })
    ));
    let too_long = format!(r"\\?\C:\{}", "x".repeat(32767));
    assert!(matches!(
        validate_sqlite_file_path(Path::new(&too_long)),
        Err(StoreError::SqlitePath { .. })
    ));
}

#[cfg(windows)]
#[test]
fn bundled_locking_long_vfs_opens_beyond_the_ordinary_vfs_capacity() {
    let directory = crate::test_support::temp_home().unwrap();
    // The safe rusqlite opener refuses an unregistered VFS. This exercises
    // the linked binary's registration without calling SQLite's raw FFI.
    drop(
        Connection::open_with_flags_and_vfs(
            sqlite_filename(&directory.path().join("vfs-registration.db")).unwrap(),
            OpenFlags::default(),
            "win32-longpath",
        )
        .expect("bundled locking long-path VFS is registered"),
    );
    let mut parent = directory.path().to_path_buf();
    while parent.to_string_lossy().len() < 1100 {
        parent.push("deep-component-for-sqlite-long-path".repeat(2));
    }
    std::fs::create_dir_all(&parent).unwrap();
    let path = parent.join("live.db");
    assert!(sqlite_filename(&path).unwrap().len() > 1040);
    let connection = open_sqlite_file(&path, OpenFlags::default()).unwrap();
    connection
        .execute_batch("CREATE TABLE proof(value INTEGER); INSERT INTO proof VALUES (9);")
        .unwrap();
    let copy = parent.join("copy.db");
    vacuum_into(&connection, &copy).unwrap();
    drop(connection);
    assert_eq!(
        open_immutable(&copy)
            .unwrap()
            .query_row("SELECT value FROM proof", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        9
    );
    assert_eq!(
        open_sqlite_file(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap()
            .query_row("SELECT value FROM proof", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        9
    );
}
