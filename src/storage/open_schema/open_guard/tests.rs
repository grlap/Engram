use std::time::Duration;

use super::{OPEN_LOCK_BUSY_REFUSAL, OpenGuard, lock_path};
use crate::storage::open_schema::lock_wait::LockWait;
use crate::storage::{StoreOpenRefusalKind, store_open_refusal_kind};

fn short_wait() -> LockWait {
    LockWait::Timeout(Duration::from_millis(50))
}

/// Takes the lock on another thread, as another opener would, and reports
/// what that opener got.
fn acquire_elsewhere(database: &std::path::Path) -> Result<(), crate::storage::StoreError> {
    let database = database.to_path_buf();
    std::thread::spawn(move || OpenGuard::acquire(&database, short_wait()).map(drop))
        .join()
        .expect("other opener")
}

#[test]
fn a_second_opener_waits_and_refuses_as_a_busy_store_while_the_first_holds_the_lock() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    let mut held = OpenGuard::acquire(&database, short_wait()).expect("first opener");
    let refused = acquire_elsewhere(&database).expect_err("the lock is held");
    assert!(
        refused.to_string().contains(OPEN_LOCK_BUSY_REFUSAL),
        "{refused}"
    );
    assert_eq!(
        store_open_refusal_kind(&refused),
        StoreOpenRefusalKind::Busy
    );
    held.release();
    acquire_elsewhere(&database).expect("released: the next opener takes it");
    // The sidecar stays, so every opener locks the same file.
    assert!(lock_path(&database).is_file());
}

#[test]
fn dropping_the_guard_releases_the_lock() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    drop(OpenGuard::acquire(&database, short_wait()).expect("first opener"));
    acquire_elsewhere(&database).expect("dropped: the next opener takes it");
}

#[test]
fn an_open_nested_on_the_same_thread_does_not_wait_for_itself() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    let mut outer = OpenGuard::acquire(&database, short_wait()).expect("outer open");
    let mut nested = OpenGuard::acquire(&database, short_wait()).expect("nested open");
    // The nested open holds nothing, so releasing it keeps the outer lock.
    nested.release();
    assert!(
        acquire_elsewhere(&database).is_err(),
        "the outer open still holds it"
    );
    outer.release();
    acquire_elsewhere(&database).expect("released by the outer open");
}

#[test]
fn a_store_reached_through_a_linked_directory_locks_the_same_sidecar() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let real = directory.path().join("real");
    std::fs::create_dir(&real).expect("store directory");
    let alias = directory.path().join("alias");
    crate::test_support::make_dir_link(&real, &alias);
    let through_real = real.join("work.sqlite3");
    let through_alias = alias.join("work.sqlite3");
    for created in [false, true] {
        if created {
            std::fs::write(&through_real, b"").expect("an existing store file");
        }
        let mut held = OpenGuard::acquire(&through_real, short_wait()).expect("first opener");
        let refused = acquire_elsewhere(&through_alias).expect_err("the alias is the same store");
        assert_eq!(
            store_open_refusal_kind(&refused),
            StoreOpenRefusalKind::Busy,
            "store file exists: {created}: {refused}"
        );
        // On the same thread, the alias is the open already held.
        drop(OpenGuard::acquire(&through_alias, short_wait()).expect("nested through the alias"));
        held.release();
        acquire_elsewhere(&through_alias).expect("released for every spelling");
    }
    crate::test_support::remove_dir_link(&alias);
}

/// Makes `link` a file link to `target`, which need not exist. `false` when
/// this Windows host does not allow this process to make one.
fn make_file_link(target: &std::path::Path, link: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).expect("file link");
        true
    }
    #[cfg(windows)]
    {
        match std::os::windows::fs::symlink_file(target, link) {
            Ok(()) => true,
            // ERROR_PRIVILEGE_NOT_HELD: no symlink privilege on this host.
            Err(error) if error.raw_os_error() == Some(1314) => false,
            Err(error) => panic!("file link: {error}"),
        }
    }
}

#[test]
fn a_link_at_the_store_name_locks_one_sidecar_before_and_after_the_store_exists() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let real = directory.path().join("real.sqlite3");
    let alias = directory.path().join("alias.sqlite3");
    if !make_file_link(&real, &alias) {
        eprintln!(
            "this host refuses file links to this process; the link case is not checked here"
        );
        return;
    }
    // The opener comes through the link while the store does not exist...
    let mut held = OpenGuard::acquire(&alias, short_wait()).expect("first opener");
    // ...and SQLite creates the store through the link while it holds the lock.
    std::fs::write(&real, b"").expect("the store created through the link");
    for spelling in [&alias, &real] {
        let refused = acquire_elsewhere(spelling).expect_err("the same store");
        assert_eq!(
            store_open_refusal_kind(&refused),
            StoreOpenRefusalKind::Busy,
            "{}: {refused}",
            spelling.display()
        );
        // On the same thread, either spelling is the open already held.
        drop(OpenGuard::acquire(spelling, short_wait()).expect("nested"));
    }
    held.release();
    acquire_elsewhere(&alias).expect("released");
    acquire_elsewhere(&real).expect("released");
}

#[test]
fn a_sidecar_this_process_may_only_read_still_locks() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    drop(OpenGuard::acquire(&database, short_wait()).expect("create the sidecar"));
    let sidecar = lock_path(&database);
    let original = std::fs::metadata(&sidecar).expect("sidecar").permissions();
    let mut read_only = original.clone();
    read_only.set_readonly(true);
    std::fs::set_permissions(&sidecar, read_only).expect("read-only sidecar");
    let held = OpenGuard::acquire(&database, short_wait());
    let other = acquire_elsewhere(&database);
    drop(held.expect("a read-only sidecar still locks"));
    // The fixture's own file gets its permissions back for its removal.
    std::fs::set_permissions(&sidecar, original).expect("original permissions");
    assert!(
        matches!(&other, Err(refused) if store_open_refusal_kind(refused) == StoreOpenRefusalKind::Busy),
        "{other:?}"
    );
}

#[test]
fn a_directory_at_the_store_path_takes_no_lock_and_leaves_no_sidecar() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let real = directory.path().join("real");
    std::fs::create_dir(&real).expect("a directory where a store would be");
    let alias = directory.path().join("alias");
    crate::test_support::make_dir_link(&real, &alias);
    for path in [&real, &alias] {
        drop(OpenGuard::acquire(path, short_wait()).expect("no lock to take"));
        assert!(!lock_path(path).exists(), "{}", path.display());
    }
    // Nothing was made beside them: only the directory and its link.
    let mut names: Vec<_> = std::fs::read_dir(directory.path())
        .expect("fixture")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    names.sort();
    assert_eq!(names, ["alias", "real"]);
    crate::test_support::remove_dir_link(&alias);
}

/// SQLite reads "file:." as a URI naming the current directory; read as a
/// path it names nothing, so a lock named after it would sit beside the
/// working directory's entries. On Windows every SQLite filename is an
/// absolute native path, never a URI.
#[cfg(unix)]
#[test]
fn a_filename_sqlite_reads_as_a_uri_takes_no_lock_and_leaves_no_sidecar() {
    let uri = std::path::Path::new("file:.");
    let stray = std::env::current_dir()
        .expect("working directory")
        .join("file:..open-lock");
    let held = OpenGuard::acquire(uri, short_wait());
    let leaked = stray.exists();
    if leaked {
        std::fs::remove_file(&stray).expect("remove the stray sidecar");
    }
    drop(held.expect("no lock to take"));
    assert!(!leaked, "a sidecar was made at {}", stray.display());
}

#[test]
fn a_store_under_a_file_takes_no_lock() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let file = directory.path().join("plain");
    std::fs::write(&file, b"").expect("a file where a directory would be");
    let database = file.join("work.sqlite3");
    drop(OpenGuard::acquire(&database, short_wait()).expect("no lock to take"));
    assert!(!lock_path(&database).exists());
}

#[test]
fn a_store_in_a_missing_directory_takes_no_lock() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("missing").join("work.sqlite3");
    drop(OpenGuard::acquire(&database, short_wait()).expect("no lock to take"));
    assert!(!lock_path(&database).exists());
    assert!(!database.parent().expect("parent").exists());
}
