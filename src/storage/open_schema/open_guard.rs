//! One writable open of a store at a time, until the store is known to be
//! in write-ahead-log mode and initialized.
//!
//! Several first openers of a new store each switch it to write-ahead
//! logging and initialize it. When they do that at once, SQLite on Windows
//! can refuse every one of them with its locking-protocol error, after about
//! ten seconds and again on the open's one retry. So a writable open takes
//! an exclusive lock on a sidecar file before it opens the store, which
//! holds across threads and processes, and gives it up as soon as the store
//! is known to be in write-ahead-log mode with its schema. It does not hold
//! it through the rest of the open's setup, which waits on SQLite's own
//! locks. The sidecar file is never removed, so every opener locks the same
//! file.
//!
//! The lock is released explicitly, never only by closing the file, so no
//! copy of the file a child process inherited can keep it held.
//!
//! The sidecar is named after the file the store's path resolves to, so
//! every spelling of one store locks one sidecar. Where no sidecar can be
//! had, or the file system cannot lock one, the open goes ahead without the
//! lock, as every open did before there was one.

use std::cell::RefCell;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::ErrorKind;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::lock_wait::LockWait;
use crate::storage::StoreError;

/// What the sidecar file's name adds to the store file's name.
const OPEN_LOCK_SUFFIX: &str = ".open-lock";

/// The message of the refusal when another opener held the lock for the
/// whole wait.
const OPEN_LOCK_BUSY_REFUSAL: &str =
    "another opener of this store is still opening it; open the store again";

thread_local! {
    /// The sidecar files this thread holds, so that an open nested in
    /// another on the same thread never waits for itself.
    static HELD: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
}

/// A writable open's hold on its store's sidecar lock; empty when the open
/// takes none. It stays on the thread that took it, where [`HELD`] records
/// it.
#[must_use = "dropping the guard releases the store's open lock"]
pub(super) struct OpenGuard {
    held: Option<(File, PathBuf)>,
    _thread: PhantomData<*const ()>,
}

impl OpenGuard {
    /// No lock: for an open that writes no file of its own, or that runs
    /// inside another that holds it.
    pub(super) const fn none() -> Self {
        Self {
            held: None,
            _thread: PhantomData,
        }
    }

    /// Takes the lock for the store at `database`, waiting for another
    /// opener as `lock_wait` allows, and refusing as a busy store when the
    /// wait ends first. A path that names a directory, a filename SQLite
    /// reads as a URI, and a store whose sidecar cannot be had or cannot be
    /// locked on its file system, take none.
    pub(super) fn acquire(database: &Path, lock_wait: LockWait) -> Result<Self, StoreError> {
        if database.as_os_str().is_empty() || database == Path::new(":memory:") {
            return Ok(Self::none());
        }
        // A path SQLite would refuse is refused as SQLite refuses it, before
        // any sidecar is made beside it.
        let filename = crate::storage::sqlite_path::sqlite_filename(database)?;
        // SQLite reads a filename that starts with "file:" as a URI and opens
        // the file the URI names, which this path does not show, so no
        // sidecar named after this path would be that store's. Such an open
        // takes none, as every open did before there was one.
        if filename.starts_with("file:") {
            #[cfg(test)]
            crate::storage::copy_trace::mark(|| "open: a URI filename takes no open lock".into());
            return Ok(Self::none());
        }
        // A path that names a directory, or a link to one, is no store, and
        // SQLite refuses to open it. A sidecar for it would sit beside the
        // directory, outside it, and nothing would ever open it again.
        if std::fs::metadata(database).is_ok_and(|metadata| metadata.is_dir()) {
            #[cfg(test)]
            crate::storage::copy_trace::mark(|| "open: a directory takes no open lock".into());
            return Ok(Self::none());
        }
        #[cfg(test)]
        super::super::before_open_lock();
        let path = lock_path(database);
        if HELD.with(|held| held.borrow().contains(&path)) {
            return Ok(Self::none());
        }
        let Some((file, read_only)) = open_sidecar(&path)? else {
            #[cfg(test)]
            crate::storage::copy_trace::mark(|| "open: no open lock to be had".into());
            return Ok(Self::none());
        };
        let limit = lock_wait.retry_limit(Instant::now());
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) => {
                    match lock_wait.retry_pause(limit, Instant::now()) {
                        Some(pause) => std::thread::sleep(pause),
                        None => return Err(open_lock_busy()),
                    }
                }
                // A file system that cannot lock, or cannot lock through a
                // handle opened only to read (an emulated lock on a network
                // file system needs one opened to write), takes none.
                Err(TryLockError::Error(error))
                    if read_only || error.kind() == ErrorKind::Unsupported =>
                {
                    #[cfg(test)]
                    crate::storage::copy_trace::mark(|| {
                        format!("open: open lock unavailable here: {error}")
                    });
                    return Ok(Self::none());
                }
                Err(TryLockError::Error(error)) => {
                    return Err(crate::storage::sqlite_path::file_io_error(&path, error));
                }
            }
        }
        HELD.with(|held| held.borrow_mut().push(path.clone()));
        #[cfg(test)]
        overlap::enter(&path);
        #[cfg(test)]
        crate::storage::copy_trace::mark(|| "open: open lock taken".into());
        Ok(Self {
            held: Some((file, path)),
            _thread: PhantomData,
        })
    }

    /// Releases the lock, if this guard holds it. Releasing twice does
    /// nothing.
    pub(super) fn release(&mut self) {
        if let Some((file, path)) = self.held.take() {
            #[cfg(test)]
            overlap::exit(&path);
            HELD.with(|held| held.borrow_mut().retain(|held| *held != path));
            // An unlock failure leaves the lock to the close below.
            let _ = file.unlock();
            #[cfg(test)]
            crate::storage::copy_trace::mark(|| "open: open lock released".into());
        }
    }
}

impl Drop for OpenGuard {
    fn drop(&mut self) {
        self.release();
    }
}

/// Opens the sidecar at `path` for locking: to read and write, creating it,
/// or only to read when it exists and this process may not write it, since
/// a lock needs only a handle; the flag says which. `None` when no sidecar
/// can be had: in a directory that does not exist, that is a file, or that
/// cannot be written.
fn open_sidecar(path: &Path) -> Result<Option<(File, bool)>, StoreError> {
    let absent = |kind| matches!(kind, ErrorKind::NotFound | ErrorKind::NotADirectory);
    let refused = |kind| {
        matches!(
            kind,
            ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem
        )
    };
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
    {
        Ok(file) => return Ok(Some((file, false))),
        Err(error) if absent(error.kind()) => return Ok(None),
        Err(error) if refused(error.kind()) => {}
        Err(error) => return Err(crate::storage::sqlite_path::file_io_error(path, error)),
    }
    match OpenOptions::new().read(true).open(path) {
        Ok(file) => Ok(Some((file, true))),
        Err(error) if absent(error.kind()) || refused(error.kind()) => Ok(None),
        Err(error) => Err(crate::storage::sqlite_path::file_io_error(path, error)),
    }
}

/// The sidecar lock file of the store at `database`, named after the file
/// the path resolves to: a link to the store or to its directory and a
/// relative path name one sidecar. A link at the store's own name is
/// followed even while its target does not exist yet, as SQLite follows it
/// to create the store, so the name stays the same once the store exists. A
/// store that does not exist yet is named by its resolved directory and its
/// own file name, in the letter case given; on a file system that ignores
/// case, two spellings of it name one sidecar file but are different names
/// to [`HELD`]. A hard link to the store is another name for the same file
/// that no path resolution can see, so it names another sidecar.
fn lock_path(database: &Path) -> PathBuf {
    let mut name = resolved_store(database).into_os_string();
    name.push(OPEN_LOCK_SUFFIX);
    PathBuf::from(name)
}

/// Links followed at most, past which a store path names its last link.
const LINK_DEPTH: usize = 40;

fn resolved_store(database: &Path) -> PathBuf {
    let mut path = database.to_path_buf();
    for _ in 0..LINK_DEPTH {
        if let Ok(real) = std::fs::canonicalize(&path) {
            return real;
        }
        let Ok(target) = std::fs::read_link(&path) else {
            break;
        };
        path = match path.parent() {
            Some(parent) if target.is_relative() => parent.join(target),
            _ => target,
        };
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    match (std::fs::canonicalize(parent), path.file_name()) {
        (Ok(parent), Some(name)) => parent.join(name),
        _ => path,
    }
}

fn open_lock_busy() -> StoreError {
    StoreError::Sqlite(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
        Some(OPEN_LOCK_BUSY_REFUSAL.into()),
    ))
}

/// How many openers held one store's lock at once, at most, for tests that
/// show its opens never overlapped.
#[cfg(test)]
pub(crate) mod overlap {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    static HOLDERS: Mutex<Option<HashMap<PathBuf, (usize, usize)>>> = Mutex::new(None);

    fn with<T>(action: impl FnOnce(&mut HashMap<PathBuf, (usize, usize)>) -> T) -> T {
        let mut holders = HOLDERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        action(holders.get_or_insert_with(HashMap::new))
    }

    pub(super) fn enter(path: &Path) {
        with(|holders| {
            let (now, most) = holders.entry(path.to_path_buf()).or_insert((0, 0));
            *now += 1;
            *most = (*most).max(*now);
        });
    }

    pub(super) fn exit(path: &Path) {
        with(|holders| {
            if let Some((now, _)) = holders.get_mut(path) {
                *now = now.saturating_sub(1);
            }
        });
    }

    /// The most openers that held the lock of the store at `database` at
    /// once, and how many hold it now.
    pub(crate) fn of(database: &Path) -> (usize, usize) {
        let path = super::lock_path(database);
        with(|holders| {
            holders
                .get(&path)
                .map_or((0, 0), |(now, most)| (*most, *now))
        })
    }
}

#[cfg(test)]
mod tests;
