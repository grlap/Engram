//! Shared fixture ownership under one inspectable OS Temp/engram directory.

use std::{
    fs, io,
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    thread,
    time::Duration,
};

/// Owns only its freshly created fixture directory, never SQLite handles.
/// Declare this before local stores (and after stores in struct field order)
/// so handles close first. Persistent cleanup errors are reported, not hidden.
pub(crate) struct TempHome(Option<tempfile::TempDir>, bool);

static ROOT_LIFETIME: Mutex<()> = Mutex::new(());

impl TempHome {
    pub(crate) fn path(&self) -> &Path {
        self.0.as_ref().expect("live fixture directory").path()
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let Some(directory) = self.0.take() else {
            return;
        };
        let path = directory.keep();
        if let Err(error) = remove_with_retry(|| fs::remove_dir_all(&path), thread::sleep) {
            let _ = writeln!(
                io::stderr().lock(),
                "Engram test fixture cleanup FAILED: {}: {error}; close all fixture handles before dropping its TempHome",
                path.display()
            );
        }
        if self.1 {
            let _lock = ROOT_LIFETIME
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Remove only our empty run directory, never siblings or leftovers.
            if let Some(root) = path.parent() {
                match fs::remove_dir(root) {
                    Ok(()) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
                        ) => {}
                    Err(error) => {
                        let _ = writeln!(
                            io::stderr().lock(),
                            "Engram test run cleanup FAILED: {}: {error}",
                            root.display()
                        );
                    }
                }
            }
        }
    }
}

fn remove_with_retry(
    mut remove: impl FnMut() -> io::Result<()>,
    mut pause: impl FnMut(Duration),
) -> io::Result<()> {
    for delay in [0, 10, 25, 50] {
        if delay != 0 {
            pause(Duration::from_millis(delay));
        }
        match remove() {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(_) => {}
        }
    }
    pause(Duration::from_millis(100));
    match remove() {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn fixture_root() -> io::Result<PathBuf> {
    static RUN_NAME: OnceLock<String> = OnceLock::new();
    let supplied = std::env::var_os("ENGRAM_TEST_RUN_ROOT").map(PathBuf::from);
    let name =
        RUN_NAME.get_or_init(|| format!("run-{}-{}", std::process::id(), uuid::Uuid::new_v4()));
    select_fixture_root(supplied.as_deref(), &std::env::temp_dir(), name)
}

fn select_fixture_root(
    supplied: Option<&Path>,
    local_temp: &Path,
    run_name: &str,
) -> io::Result<PathBuf> {
    let root = supplied.map_or_else(
        || local_temp.join("engram").join(run_name),
        Path::to_path_buf,
    );
    if !root.is_absolute()
        || root.parent().and_then(Path::file_name) != Some(std::ffi::OsStr::new("engram"))
        || !root
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("run-"))
        || root
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(io::Error::other(format!(
            "test run root must be an absolute engram/run-* child: {}",
            root.display()
        )));
    }
    Ok(root)
}

#[test]
fn fixture_root_selection_uses_the_launcher_policy_without_rederiving_temp() {
    let base = std::env::temp_dir();
    let local = base.join("rust-temp-policy");
    let launcher = base
        .join("node-temp-policy")
        .join("engram")
        .join("run-123-random");
    assert_eq!(
        select_fixture_root(Some(&launcher), &local, "run-unused").unwrap(),
        launcher
    );
    assert_eq!(
        select_fixture_root(None, &local, "run-456-random").unwrap(),
        local.join("engram/run-456-random")
    );
    for invalid in [
        PathBuf::from("engram/run-relative"),
        base.join("other/run-wrong-parent"),
        base.join("engram/not-a-run"),
        base.join("engram/../engram/run-traversal"),
    ] {
        assert!(
            select_fixture_root(Some(&invalid), &local, "run-unused").is_err(),
            "{}",
            invalid.display()
        );
    }
}

/// Creates a unique fixture only under the dedicated Temp/engram root.
pub(crate) fn temp_home() -> io::Result<TempHome> {
    let _lock = ROOT_LIFETIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = fixture_root()?;
    let product_root = root.parent().expect("validated parent");
    // Refuse a pre-existing product-root symlink before creating its run child.
    refuse_symlink(product_root)?;
    refuse_symlink(&root)?;
    fs::create_dir_all(&root)?;
    refuse_symlink(product_root)?;
    refuse_symlink(&root)?;
    tempfile::Builder::new()
        .prefix("engram-rust-")
        .tempdir_in(root)
        .map(|directory| {
            TempHome(
                Some(directory),
                std::env::var_os("ENGRAM_TEST_RUN_ROOT").is_none(),
            )
        })
}

fn refuse_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::other(format!(
            "test fixture root must not be a symlink: {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[test]
fn fixture_home_stays_under_engram_and_is_removed_after_handles_close() {
    let directory = temp_home().unwrap();
    let path = directory.path().to_owned();
    assert_eq!(path.parent(), Some(fixture_root().unwrap().as_path()));
    let store = rusqlite::Connection::open(path.join("work.db")).unwrap();
    store
        .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE fixture(value TEXT);")
        .unwrap();
    drop(store);
    drop(directory);
    assert!(!path.exists());
}

#[cfg(windows)]
#[test]
fn fixture_open_sqlite_handle_prevents_removal_until_owner_drops() {
    let owner = temp_home().unwrap();
    let raw = tempfile::Builder::new()
        .prefix("sharing-repro-")
        .tempdir_in(owner.path())
        .unwrap();
    let path = raw.path().to_owned();
    let store = rusqlite::Connection::open(path.join("work.db")).unwrap();
    store
        .execute_batch("CREATE TABLE fixture(value TEXT);")
        .unwrap();
    let sharing_error = fs::remove_file(path.join("work.db")).unwrap_err();
    assert_eq!(sharing_error.raw_os_error(), Some(32));
    let error = raw
        .close()
        .expect_err("an open SQLite handle denies directory removal on Windows");
    eprintln!(
        "Reproduced TempDir removal failure at {}: {error}; OS error {:?}",
        path.display(),
        sharing_error.raw_os_error()
    );
    assert!(path.exists());
    drop(store);
    remove_with_retry(|| fs::remove_dir_all(&path), thread::sleep).unwrap();
    assert!(!path.exists());
}

#[test]
fn fixture_removal_retries_transient_errors_and_returns_persistent_failure() {
    let mut attempts = 0;
    let mut pauses = Vec::new();
    remove_with_retry(
        || {
            attempts += 1;
            if attempts < 3 {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            } else {
                Ok(())
            }
        },
        |delay| pauses.push(delay),
    )
    .unwrap();
    assert_eq!(attempts, 3);
    assert_eq!(
        pauses,
        [Duration::from_millis(10), Duration::from_millis(25)]
    );
    let mut failures = 0;
    let error = remove_with_retry(
        || {
            failures += 1;
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        },
        |_| {},
    )
    .unwrap_err();
    assert_eq!(failures, 5);
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
}
