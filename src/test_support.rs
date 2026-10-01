//! Shared fixture ownership under this repository's `target/tmp/engram`.
//!
//! Fixtures never use the system Temp folder, and every recursive delete goes
//! through [`remove_fixture_dir`], which refuses any path outside its root.

use std::{
    fs, io,
    io::Write,
    path::{Component, Path, PathBuf},
    sync::{Mutex, OnceLock},
    thread,
    time::Duration,
};

/// Owns only its freshly created fixture directory, never SQLite handles.
/// Declare this before local stores (and after stores in struct field order)
/// so handles close first. Persistent cleanup errors are reported, not hidden.
pub(crate) struct TempHome {
    directory: Option<tempfile::TempDir>,
    /// The `target/tmp/engram` it was created under; teardown is guarded by
    /// the same anchor.
    anchor: PathBuf,
    /// Whether this process created the run directory and removes it.
    owns_run: bool,
}

static ROOT_LIFETIME: Mutex<()> = Mutex::new(());

impl TempHome {
    pub(crate) fn path(&self) -> &Path {
        self.directory
            .as_ref()
            .expect("live fixture directory")
            .path()
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let Some(directory) = self.directory.take() else {
            return;
        };
        let failures = release_fixture_home(&self.anchor, &directory.keep(), self.owns_run);
        for failure in failures {
            let _ = writeln!(io::stderr().lock(), "{failure}");
        }
    }
}

/// Removes a fixture home under `anchor`, then its run directory when this
/// process created that and it is now empty. Returns each cleanup failure
/// for the caller to report, never hides one.
fn release_fixture_home(anchor: &Path, path: &Path, owns_run: bool) -> Vec<String> {
    let mut failures = Vec::new();
    let run_root = path.parent().expect("fixture home lies in its run root");
    let refused = match remove_fixture_dir_in(anchor, run_root, path) {
        Ok(()) => false,
        Err(error) => {
            failures.push(format!(
                "Engram test fixture cleanup FAILED: {}: {error}; close all fixture handles before dropping its TempHome",
                path.display()
            ));
            is_refusal(&error)
        }
    };
    // After a refusal the run directory is not known to be ours: leave it.
    if owns_run && !refused {
        let _lock = ROOT_LIFETIME
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Err(error) = remove_empty_run_dir_in(anchor, run_root) {
            failures.push(format!(
                "Engram test run cleanup FAILED: {}: {error}",
                run_root.display()
            ));
        }
    }
    failures
}

/// A deletion the guard refused. It is never retried: a retry cannot make an
/// outside path acceptable.
#[derive(Debug)]
struct Refusal(String);

impl std::fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for Refusal {}

fn refusal(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, Refusal(message))
}

fn is_refusal(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.downcast_ref::<Refusal>().is_some())
}

/// Removes our run directory when it is empty, never recursively, and only
/// when it is a direct child of `anchor` (this repository's
/// `target/tmp/engram`) reached without any link from `target/` down.
/// A directory that is gone or still holds entries is left as it is.
fn remove_empty_run_dir_in(anchor: &Path, run_root: &Path) -> io::Result<()> {
    remove_empty_run_dir_with(anchor, run_root, |path| fs::remove_dir(path))
}

fn remove_empty_run_dir_with(
    anchor: &Path,
    run_root: &Path,
    remove: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let refuse = |reason: &str| {
        refusal(format!(
            "refusing to delete {}: {reason} (anchor {})",
            run_root.display(),
            anchor.display()
        ))
    };
    if run_root.parent() != Some(anchor)
        || !run_root
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("run-"))
        || run_root
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Err(refuse("it is not a run directory directly in the anchor"));
    }
    let tmp = anchor.parent().expect("anchor lies in target/tmp");
    for step in [tmp.parent().expect("tmp lies in target"), tmp, anchor] {
        match fs::symlink_metadata(step) {
            Ok(metadata) if !metadata.file_type().is_symlink() => {}
            Ok(_) => return Err(refuse("a step down to the anchor is a link")),
            Err(error) => return Err(error),
        }
    }
    match fs::symlink_metadata(run_root) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(refuse("the run directory is a link"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    let resolved_parent = fs::canonicalize(run_root)?.parent().map(Path::to_path_buf);
    if resolved_parent != Some(fs::canonicalize(anchor)?) {
        return Err(refuse("it resolves outside the anchor"));
    }
    // Parallel fixtures may pin this shared ancestor without delete sharing.
    // Leave a populated run alone before attempting removal: on Windows an
    // open handle can otherwise mask DirectoryNotEmpty with SharingViolation.
    let populated = match fs::read_dir(run_root) {
        Ok(mut entries) => entries.next().transpose()?.is_some(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if populated {
        return Ok(());
    }
    match remove(run_root) {
        // Another fixture can populate and pin the run after our first read.
        // Only a confirmed populated directory makes this race benign;
        // an empty locked run still reports the original teardown failure.
        #[cfg(windows)]
        Err(error) if error.raw_os_error() == Some(32) => match fs::read_dir(run_root) {
            Ok(mut entries) => {
                if entries.next().is_some_and(|entry| entry.is_ok()) {
                    Ok(())
                } else {
                    Err(error)
                }
            }
            _ => Err(error),
        },
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
        result => result,
    }
}

/// Removes `path` and everything below it, but only when `root` lies below
/// this repository's `target/tmp/engram`, `path` lies strictly below `root`,
/// and no step from `target/` down to `path` is a symlink or junction.
/// Anything else is refused before a single file is deleted.
pub(crate) fn remove_fixture_dir(root: &Path, path: &Path) -> io::Result<()> {
    remove_fixture_dir_in(&product_root(), root, path)
}

fn remove_fixture_dir_in(anchor: &Path, root: &Path, path: &Path) -> io::Result<()> {
    if !fixture_dir_to_remove(anchor, root, path)? {
        return Ok(());
    }
    // Check again on every attempt, so a step replaced by a link between
    // attempts is refused rather than followed. What remains is the moment
    // between the last check and the delete itself. A refusal ends the
    // retries at once.
    remove_with_retry(
        || {
            if fixture_dir_to_remove(anchor, root, path)? {
                fs::remove_dir_all(path)
            } else {
                Ok(())
            }
        },
        thread::sleep,
    )
}

/// Whether `path` exists and may be removed; refusals are errors.
fn fixture_dir_to_remove(anchor: &Path, root: &Path, path: &Path) -> io::Result<bool> {
    let refuse = |reason: &str| {
        refusal(format!(
            "refusing to delete {}: {reason} (fixture root {}, anchor {})",
            path.display(),
            root.display(),
            anchor.display()
        ))
    };
    // Components, not string prefixes: `target\tmp-evil` is not below
    // `target\tmp`.
    let rest = path
        .strip_prefix(root)
        .map_err(|_| refuse("it is not below the fixture root"))?;
    if rest.as_os_str().is_empty() {
        return Err(refuse("it is the fixture root itself"));
    }
    if path
        .components()
        .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Err(refuse("its path is not normalized"));
    }
    // `is_symlink` is true for Windows junctions as well as symlinks.
    let is_link = |step: &Path| -> io::Result<Option<bool>> {
        match fs::symlink_metadata(step) {
            Ok(metadata) => Ok(Some(metadata.file_type().is_symlink())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    };
    // The anchor and its two parents must exist and must not be links.
    let tmp = anchor.parent().expect("anchor lies in target/tmp");
    for step in [tmp.parent().expect("tmp lies in target"), tmp, anchor] {
        match is_link(step)? {
            Some(false) => {}
            Some(true) => return Err(refuse("a step down to the anchor is a link")),
            None => return Err(refuse("the anchor does not exist")),
        }
    }
    let resolved_anchor = fs::canonicalize(anchor)?;
    // Walk up from the path to the anchor, refusing any link on the way,
    // before resolving: canonicalize would follow a link and report its
    // destination. Walking up covers links above `root` too.
    let mut current = path;
    loop {
        match is_link(current)? {
            Some(true) => return Err(refuse("a step on the way to it is a link")),
            Some(false) => {}
            None if current == path => return Ok(false),
            None => return Err(refuse("a step on the way to it does not exist")),
        }
        if fs::canonicalize(current)? == resolved_anchor {
            break;
        }
        current = current
            .parent()
            .ok_or_else(|| refuse("it is not below target/tmp/engram"))?;
    }
    let resolved_root = fs::canonicalize(root)?;
    if resolved_root == resolved_anchor || !resolved_root.starts_with(&resolved_anchor) {
        return Err(refuse("the fixture root is not below target/tmp/engram"));
    }
    let resolved = fs::canonicalize(path)?;
    if resolved == resolved_root || !resolved.starts_with(&resolved_root) {
        return Err(refuse("it resolves outside the fixture root"));
    }
    Ok(true)
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
            Err(error) if is_refusal(&error) => return Err(error),
            Err(_) => {}
        }
    }
    pause(Duration::from_millis(100));
    match remove() {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// `target/tmp/engram` of the repository this code was compiled from.
fn product_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("tmp")
        .join("engram")
}

fn fixture_root() -> io::Result<PathBuf> {
    static RUN_NAME: OnceLock<String> = OnceLock::new();
    let supplied = std::env::var_os("ENGRAM_TEST_RUN_ROOT").map(PathBuf::from);
    let name = RUN_NAME.get_or_init(|| {
        let id = uuid::Uuid::new_v4().simple().to_string();
        format!("run-{}-{}", std::process::id(), &id[..8])
    });
    select_fixture_root(supplied.as_deref(), &product_root(), name)
}

fn select_fixture_root(
    supplied: Option<&Path>,
    product_root: &Path,
    run_name: &str,
) -> io::Result<PathBuf> {
    let root = supplied.map_or_else(|| product_root.join(run_name), Path::to_path_buf);
    if !root.is_absolute()
        || root.parent().and_then(Path::file_name) != Some(std::ffi::OsStr::new("engram"))
        || !root
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("run-"))
        || root
            .components()
            .any(|part| matches!(part, Component::ParentDir))
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
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
    let local = target.join("rust-policy").join("engram");
    let launcher = target
        .join("node-policy")
        .join("engram")
        .join("run-123-random");
    assert_eq!(
        select_fixture_root(Some(&launcher), &local, "run-unused").unwrap(),
        launcher
    );
    assert_eq!(
        select_fixture_root(None, &local, "run-456-random").unwrap(),
        local.join("run-456-random")
    );
    for invalid in [
        PathBuf::from("engram/run-relative"),
        target.join("other/run-wrong-parent"),
        target.join("engram/not-a-run"),
        target.join("engram/../engram/run-traversal"),
    ] {
        assert!(
            select_fixture_root(Some(&invalid), &local, "run-unused").is_err(),
            "{}",
            invalid.display()
        );
    }
}

/// Creates a unique fixture only under this repository's target/tmp/engram.
pub(crate) fn temp_home() -> io::Result<TempHome> {
    let _lock = ROOT_LIFETIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let owns_run = std::env::var_os("ENGRAM_TEST_RUN_ROOT").is_none();
    temp_home_in(&product_root(), &fixture_root()?, owns_run)
}

/// Creates a unique fixture in the run directory `root`, which must lie
/// directly in `product_root` (a `target/tmp/engram`). Nothing is created
/// until `target/`, `target/tmp` and `product_root` are known not to be links.
fn temp_home_in(product_root: &Path, root: &Path, owns_run: bool) -> io::Result<TempHome> {
    let tmp = product_root.parent().expect("product root lies in tmp");
    let target = tmp.parent().expect("tmp lies in target");
    // Refuse a pre-existing link on the way down before creating anything.
    for path in [target, tmp, product_root] {
        refuse_symlink(path)?;
    }
    fs::create_dir_all(product_root)?;
    // A launcher-supplied root must be this repository's own run directory,
    // however its path is spelled.
    let supplied_parent = root.parent().expect("validated parent");
    if fs::canonicalize(supplied_parent).ok() != Some(fs::canonicalize(product_root)?) {
        return Err(io::Error::other(format!(
            "test run root must lie in {}: {}",
            product_root.display(),
            root.display()
        )));
    }
    refuse_symlink(root)?;
    fs::create_dir_all(root)?;
    for path in [target, tmp, product_root, root] {
        refuse_symlink(path)?;
    }
    tempfile::Builder::new()
        .prefix("engram-rust-")
        .tempdir_in(root)
        .map(|directory| TempHome {
            directory: Some(directory),
            anchor: product_root.to_path_buf(),
            owns_run,
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
    assert!(
        fs::canonicalize(&path).unwrap().starts_with(
            fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("target")).unwrap()
        ),
        "fixture must lie in this repository's target/: {}",
        path.display()
    );
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
fn fixture_root_leaves_windows_path_headroom() {
    // The deepest SQLite files (control-policy -wal/-shm) sit about 126
    // characters below the run root, so a longer root would push them past
    // MAX_PATH. Fail here, at setup, instead of inside SQLite.
    let root = fixture_root().unwrap();
    assert!(
        root.as_os_str().len() <= 130,
        "fixture run root is too long for Windows paths ({} characters): {}",
        root.as_os_str().len(),
        root.display()
    );
}

#[test]
fn fixture_removal_refuses_paths_outside_its_root() {
    let home = temp_home().unwrap();
    let root = home.path().join("root");
    let inside = root.join("inside");
    fs::create_dir_all(inside.join("nested")).unwrap();
    let sibling = home.path().join("sibling");
    fs::create_dir(&sibling).unwrap();
    // Shares the root's name as a string prefix but is not below it.
    let collision = home.path().join("root-evil");
    fs::create_dir(&collision).unwrap();
    for refused in [
        sibling.clone(),
        root.join("..").join("sibling"),
        collision.clone(),
        root.clone(),
        PathBuf::from("relative"),
    ] {
        assert_refused(&remove_fixture_dir(&root, &refused).unwrap_err());
    }
    assert!(sibling.exists() && collision.exists() && inside.exists());

    // A link inside the root that leads outside it is refused, and so is
    // anything reached through it.
    let link = root.join("link");
    make_dir_link(&sibling, &link);
    fs::write(sibling.join("kept.txt"), "outside the root").unwrap();
    for refused in [link.clone(), link.join("kept.txt")] {
        assert_refused(&remove_fixture_dir(&root, &refused).unwrap_err());
    }
    assert!(sibling.join("kept.txt").exists());
    remove_dir_link(&link);

    remove_fixture_dir(&root, &inside).unwrap();
    assert!(!inside.exists() && root.exists());
    // A path that is already gone is not an error.
    remove_fixture_dir(&root, &inside).unwrap();
}

#[test]
fn fixture_removal_is_anchored_to_the_repository_target() {
    // Set up the fixture first: it refuses a linked target/, so the probe
    // below is never written through one.
    let home = temp_home().unwrap();
    // A victim inside this repository but outside target/tmp/engram. The
    // real callers pass the path's own parent as its root; that must not
    // turn the guard into a pass for any path.
    let id = uuid::Uuid::new_v4().simple().to_string();
    let probe = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!("guard-probe-{}", &id[..8]));
    let victim = probe.join("victim");
    let inner = victim.join("inner");
    fs::create_dir_all(&inner).unwrap();
    fs::write(victim.join("kept.txt"), "outside the anchor").unwrap();
    for (root, path) in [(&probe, &victim), (&victim, &inner)] {
        assert_refused(&remove_fixture_dir(root, path).unwrap_err());
    }
    assert!(victim.join("kept.txt").exists() && inner.exists());

    // A junction above the given root: the root and path read as below the
    // anchor but really lie outside it.
    let linked = home.path().join("linked");
    make_dir_link(&probe, &linked);
    let error = remove_fixture_dir(&linked.join("victim"), &linked.join("victim").join("inner"))
        .unwrap_err();
    assert_refused(&error);
    assert!(inner.exists());
    remove_dir_link(&linked);

    // The anchor itself is not a fixture root.
    assert_refused(&remove_fixture_dir(&product_root(), home.path()).unwrap_err());
    assert!(home.path().exists());

    // Clean up the probe one entry at a time: no recursive delete outside
    // the guard.
    fs::remove_file(victim.join("kept.txt")).unwrap();
    fs::remove_dir(&inner).unwrap();
    fs::remove_dir(&victim).unwrap();
    fs::remove_dir(&probe).unwrap();
}

#[test]
fn fixture_setup_refuses_a_linked_target_before_creating_anything() {
    // A copy of the target/tmp/engram layout inside a fixture home, so links
    // can be placed in it without touching the shared anchor.
    let home = temp_home().unwrap();
    for linked_step in ["target", "tmp"] {
        let layout = home.path().join(format!("layout-{linked_step}"));
        let victim = home.path().join(format!("victim-{linked_step}"));
        fs::create_dir(&victim).unwrap();
        let target = layout.join("target");
        let link = if linked_step == "target" {
            fs::create_dir(&layout).unwrap();
            target.clone()
        } else {
            fs::create_dir_all(&target).unwrap();
            target.join("tmp")
        };
        make_dir_link(&victim, &link);
        let product_root = target.join("tmp").join("engram");
        let error = temp_home_in(&product_root, &product_root.join("run-probe"), true)
            .err()
            .expect("a linked step above the fixture is refused");
        assert!(
            error.to_string().contains("must not be a symlink"),
            "{error}"
        );
        assert_eq!(
            fs::read_dir(&victim).unwrap().count(),
            0,
            "nothing may be created through the linked {linked_step}"
        );
        remove_dir_link(&link);
    }
}

#[test]
fn fixture_teardown_removes_only_its_own_empty_run_directory() {
    // A copy of the target/tmp/engram layout inside a fixture home, so links
    // can be swapped into it without touching the shared anchor.
    let home = temp_home().unwrap();
    let target = home.path().join("target");
    let tmp = target.join("tmp");
    let anchor = tmp.join("engram");
    let run = anchor.join("run-probe");
    let fixture = run.join("engram-rust-probe");
    fs::create_dir_all(&fixture).unwrap();

    for refused in [
        home.path().join("run-probe"),
        anchor.join("not-a-run"),
        run.join("run-nested"),
        anchor.join(".."),
    ] {
        assert_refused(&remove_empty_run_dir_in(&anchor, &refused).unwrap_err());
    }
    // A run directory that still holds entries is left in place.
    remove_empty_run_dir_in(&anchor, &run).unwrap();
    assert!(fixture.exists());

    // The victim is an empty run directory outside the anchor, reached once
    // target/tmp is swapped for a link to it. The fixture home's removal is
    // refused, and teardown must then leave the run directory alone.
    let victim = home.path().join("victim");
    let victim_run = victim.join("engram").join("run-probe");
    fs::create_dir_all(&victim_run).unwrap();
    let tmp_real = target.join("tmp-real");
    fs::rename(&tmp, &tmp_real).unwrap();
    make_dir_link(&victim, &tmp);
    // One failure, the refusal: the run directory's removal was not tried.
    let failures = release_fixture_home(&anchor, &fixture, true);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(
        failures[0].starts_with("Engram test fixture cleanup FAILED: ")
            && failures[0].contains("refusing to delete "),
        "{failures:?}"
    );
    assert!(victim_run.exists());
    // The empty-run removal refuses on its own as well.
    assert_refused(&remove_empty_run_dir_in(&anchor, &run).unwrap_err());
    assert!(victim_run.exists());
    remove_dir_link(&tmp);
    fs::rename(&tmp_real, &tmp).unwrap();

    // A run directory that is itself a link is refused.
    let run_link = anchor.join("run-link");
    make_dir_link(&victim_run, &run_link);
    assert_refused(&remove_empty_run_dir_in(&anchor, &run_link).unwrap_err());
    assert!(victim_run.exists());
    remove_dir_link(&run_link);

    // Without links, teardown removes the fixture home and then its empty
    // run directory, and nothing above it.
    assert_eq!(
        release_fixture_home(&anchor, &fixture, true),
        Vec::<String>::new()
    );
    assert!(!fixture.exists() && !run.exists() && anchor.exists());
    // A run directory that is already gone is not an error.
    remove_empty_run_dir_in(&anchor, &run).unwrap();
}

#[cfg(windows)]
#[test]
fn populated_run_with_pinned_ancestor_is_left_alone_but_empty_lock_is_reported() {
    let home = temp_home().unwrap();
    let anchor = home.path().join("target/tmp/engram");
    let run = anchor.join("run-pinned");
    let fixture = run.join("other-fixture");
    fs::create_dir_all(&fixture).unwrap();
    let pinned = cap_std::fs::Dir::open_ambient_dir(&run, cap_std::ambient_authority()).unwrap();
    assert_eq!(fs::remove_dir(&run).unwrap_err().raw_os_error(), Some(32));
    remove_empty_run_dir_in(&anchor, &run).unwrap();
    assert!(fixture.exists());
    fs::remove_dir(&fixture).unwrap();
    assert_eq!(
        remove_empty_run_dir_in(&anchor, &run)
            .unwrap_err()
            .raw_os_error(),
        Some(32)
    );
    drop(pinned);
    remove_empty_run_dir_in(&anchor, &run).unwrap();
    assert!(!run.exists());
}

#[cfg(windows)]
#[test]
fn run_populated_and_pinned_between_empty_check_and_removal_is_left_alone() {
    let home = temp_home().unwrap();
    let anchor = home.path().join("target/tmp/engram");
    let run = anchor.join("run-raced");
    fs::create_dir_all(&run).unwrap();
    let fixture = run.join("other-fixture");
    let mut pinned = None;
    remove_empty_run_dir_with(&anchor, &run, |path| {
        fs::create_dir(&fixture).unwrap();
        pinned =
            Some(cap_std::fs::Dir::open_ambient_dir(path, cap_std::ambient_authority()).unwrap());
        let result = fs::remove_dir(path);
        assert_eq!(result.as_ref().unwrap_err().raw_os_error(), Some(32));
        result
    })
    .unwrap();
    assert!(fixture.exists());
    drop(pinned);
    fs::remove_dir(fixture).unwrap();
    remove_empty_run_dir_in(&anchor, &run).unwrap();
    assert!(!run.exists());
}

/// A guard refusal, not an ordinary I/O failure, with its message intact.
fn assert_refused(error: &io::Error) {
    assert!(is_refusal(error), "not a refusal: {error:?}");
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert!(
        error.to_string().starts_with("refusing to delete "),
        "{error}"
    );
}

#[cfg(windows)]
pub(crate) fn make_dir_link(target: &Path, link: &Path) {
    // A junction needs no symlink privilege on Windows. Node creates it
    // directly from its arguments; no shell parses the paths, so `&`, `%` or
    // `^` in a checkout path stay literal.
    let output = std::process::Command::new("node")
        .arg("-e")
        .arg("require('fs').symlinkSync(process.argv[1], process.argv[2], 'junction')")
        .arg("--")
        .arg(target)
        .arg(link)
        .output()
        .expect("node creates test junctions");
    assert!(
        output.status.success(),
        "creating junction {} -> {} failed: {output:?}",
        link.display(),
        target.display()
    );
}

#[test]
fn test_links_keep_shell_characters_in_paths_literal() {
    // A folder name that a shell would split at `&` or expand at `%PATH%`.
    let home = temp_home().unwrap();
    let odd = home.path().join("a&b %PATH% ^c");
    let target = odd.join("target");
    fs::create_dir_all(&target).unwrap();
    let link = odd.join("link");
    make_dir_link(&target, &link);
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::canonicalize(&link).unwrap(),
        fs::canonicalize(&target).unwrap()
    );
    // Nothing appeared where a split or expanded command would have written.
    let names = |path: &Path| {
        let mut names: Vec<_> = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        names
    };
    assert_eq!(names(home.path()), ["a&b %PATH% ^c"]);
    assert_eq!(names(&odd), ["link", "target"]);
    remove_dir_link(&link);
}

#[cfg(not(windows))]
pub(crate) fn make_dir_link(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[cfg(windows)]
pub(crate) fn remove_dir_link(link: &Path) {
    fs::remove_dir(link).unwrap();
}

#[cfg(not(windows))]
pub(crate) fn remove_dir_link(link: &Path) {
    fs::remove_file(link).unwrap();
}

#[cfg(windows)]
#[test]
fn fixture_open_sqlite_handle_prevents_removal_until_owner_drops() {
    let owner = temp_home().unwrap();
    let path = owner.path().join("sharing-repro");
    fs::create_dir(&path).unwrap();
    let store = rusqlite::Connection::open(path.join("work.db")).unwrap();
    store
        .execute_batch("CREATE TABLE fixture(value TEXT);")
        .unwrap();
    let sharing_error = fs::remove_file(path.join("work.db")).unwrap_err();
    assert_eq!(sharing_error.raw_os_error(), Some(32));
    let error = remove_fixture_dir(owner.path(), &path)
        .expect_err("an open SQLite handle denies directory removal on Windows");
    // A sharing violation is retried and reported, never taken for a refusal.
    assert!(!is_refusal(&error), "{error}");
    eprintln!(
        "Reproduced fixture removal failure at {}: {error}; OS error {:?}",
        path.display(),
        sharing_error.raw_os_error()
    );
    assert!(path.exists());
    drop(store);
    remove_fixture_dir(owner.path(), &path).unwrap();
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
    let mut persistent_pauses = Vec::new();
    let error = remove_with_retry(
        || {
            failures += 1;
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        },
        |delay| persistent_pauses.push(delay),
    )
    .unwrap_err();
    assert_eq!(failures, 5);
    assert_eq!(
        persistent_pauses,
        [
            Duration::from_millis(10),
            Duration::from_millis(25),
            Duration::from_millis(50),
            Duration::from_millis(100)
        ]
    );
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert!(!is_refusal(&error));

    // A refusal is returned at once: no retry can make its path acceptable.
    let mut refusals = 0;
    let mut refusal_pauses = Vec::new();
    let error = remove_with_retry(
        || {
            refusals += 1;
            Err(refusal("refusing to delete probe: test".to_owned()))
        },
        |delay| refusal_pauses.push(delay),
    )
    .unwrap_err();
    assert_eq!(refusals, 1);
    assert!(refusal_pauses.is_empty(), "{refusal_pauses:?}");
    assert_refused(&error);
}
