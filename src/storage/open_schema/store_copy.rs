//! A verified copy of an existing store, taken for a backup capture without a
//! write transaction on the live store.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;

use super::{
    CanonicalObject, Connection, Duration, ObjectId, Path, SqliteStore, StoreError, store_sidecars,
};
use crate::{FeedId, ProjectId, WorkGraphSnapshotCut};

/// How many SQLite virtual-machine steps pass between two deadline checks.
const PROGRESS_STEPS: std::ffi::c_int = 1000;

/// The longest a copy connection waits for a lock, as ordinary opens do.
const LOCK_WAIT: Duration = Duration::from_secs(5);

/// The points at which a copy checks its deadline while SQLite or the hash
/// runs; a test may fire the interrupt at one of them on purpose.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CopyProbePoint {
    /// A progress check while a store or the staged copy is admitted.
    Admit,
    /// A progress check while the live store is being copied.
    Copy,
    /// A progress check while the staged copy's journal is settled.
    Settle,
    /// A progress check during the full check of the staged copy.
    Scan,
    /// A chunk of the staged copy's hash.
    Hash,
}

/// One deadline shared by every step of a copy. Every SQLite connection a
/// copy opens checks it through a progress handler and is interrupted once it
/// has passed; lock waits are bounded by the time left; the steps between
/// statements, and closing a connection, are checked explicitly. It is checked, not enforced to the instant: a file-system call
/// that stalls is not interrupted. Once a check has seen the deadline pass,
/// the interrupt stays fired, so a step that turns the interruption into some
/// other error can still be recognized as the deadline's.
#[derive(Clone)]
pub struct CopyInterrupt {
    /// `None` when the deadline lies beyond what the clock can represent.
    deadline: Option<Instant>,
    fired: Arc<AtomicBool>,
    #[cfg(test)]
    probe: Option<Arc<dyn Fn(CopyProbePoint) -> bool + Send + Sync>>,
}

impl std::fmt::Debug for CopyInterrupt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CopyInterrupt")
            .field("deadline", &self.deadline)
            .field("fired", &self.fired())
            .finish_non_exhaustive()
    }
}

impl CopyInterrupt {
    /// An interrupt that fires once `limit` has passed from now.
    #[must_use]
    pub fn after(limit: Duration) -> Self {
        Self {
            deadline: Instant::now().checked_add(limit),
            fired: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            probe: None,
        }
    }

    /// Runs `probe` at each point a test may fire the interrupt; a probe
    /// that answers `true` fires it there.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_probe(
        mut self,
        probe: Arc<dyn Fn(CopyProbePoint) -> bool + Send + Sync>,
    ) -> Self {
        self.probe = Some(probe);
        self
    }

    /// Whether the deadline has passed; a pass fires the interrupt for good.
    #[must_use]
    pub fn expired(&self) -> bool {
        if self.fired.load(Ordering::SeqCst) {
            return true;
        }
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.fired.store(true, Ordering::SeqCst);
            return true;
        }
        false
    }

    /// Whether any check has seen the deadline pass.
    #[must_use]
    pub fn fired(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }

    /// How long a lock wait may last: the time left, at most [`LOCK_WAIT`].
    fn lock_wait(&self) -> Duration {
        self.deadline.map_or(LOCK_WAIT, |deadline| {
            deadline
                .saturating_duration_since(Instant::now())
                .min(LOCK_WAIT)
        })
    }

    #[cfg(test)]
    fn probed(&self, point: CopyProbePoint) -> bool {
        if self.probe.as_ref().is_some_and(|probe| probe(point)) {
            self.fired.store(true, Ordering::SeqCst);
        }
        self.expired()
    }

    #[cfg(not(test))]
    fn probed(&self, _point: CopyProbePoint) -> bool {
        self.expired()
    }

    fn checked(&self) -> Result<(), StoreError> {
        if self.expired() {
            Err(StoreError::InvalidWork(
                "the copy passed its deadline".into(),
            ))
        } else {
            Ok(())
        }
    }

    /// Checks the deadline between two chunks of the staged copy's hash.
    pub(super) fn hash_chunk_expired(&self) -> bool {
        self.probed(CopyProbePoint::Hash)
    }

    pub(super) fn install(
        &self,
        connection: &Connection,
        point: CopyProbePoint,
    ) -> Result<(), StoreError> {
        let interrupt = self.clone();
        connection.progress_handler(PROGRESS_STEPS, Some(move || interrupt.probed(point)))?;
        Ok(())
    }
}

/// What the check of a staged copy found: its bytes, the schema it carries and
/// the project cut read from the copy itself.
#[derive(Clone, Debug)]
pub struct VerifiedStoreCopy {
    pub file_sha256: String,
    pub file_bytes: u64,
    pub schema_reference: ObjectId,
    pub cut: WorkGraphSnapshotCut,
}

impl SqliteStore {
    /// Writes a copy of the existing store at `source` to `target` through
    /// SQLite's `VACUUM INTO`, on a read-only connection that holds one read
    /// transaction and no write transaction. The store is admitted first the
    /// ordinary read-only way, so a missing, empty or different-build store is
    /// refused and no store is created at `source`. The store's content is
    /// never written, but as on any read-only open SQLite may create its
    /// shared-memory and empty log sidecars beside it, owned by the user who
    /// runs the copy, and remove the log again when the last connection
    /// closes. The connection is closed on return. A copy left behind by a
    /// failure is the caller's to remove.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the store is refused, the copy fails, or
    /// `interrupt` fires during the copy.
    pub fn copy_existing_read_only(
        source: &Path,
        target: &Path,
        interrupt: &CopyInterrupt,
    ) -> Result<(), StoreError> {
        interrupt.checked()?;
        // No `query_only`: SQLite refuses `VACUUM INTO` under it, and the
        // read-only open already rules out any write to the store.
        let connection = Self::open_existing_read_only_connection(source)?;
        interrupt.install(&connection, CopyProbePoint::Admit)?;
        let store =
            Self::from_connection_with_busy_timeout(connection, None, None, interrupt.lock_wait())?;
        interrupt.checked()?;
        store.connection.busy_timeout(interrupt.lock_wait())?;
        interrupt.install(&store.connection, CopyProbePoint::Copy)?;
        store
            .connection
            .execute("VACUUM INTO ?1", [target.to_string_lossy().as_ref()])?;
        drop(store);
        interrupt.checked()
    }

    /// Settles a copy that [`Self::copy_existing_read_only`] wrote, so it is
    /// one self-contained file, then checks it in full as
    /// [`Self::verify_backup`] does and reads, from the copy itself, the
    /// schema it carries and the cut of `project`: the project work-feed
    /// head and the project-memory change position.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the copy cannot be settled, fails its check,
    /// or `interrupt` fires.
    pub fn verify_store_copy(
        path: &Path,
        project: &ProjectId,
        interrupt: &CopyInterrupt,
    ) -> Result<VerifiedStoreCopy, StoreError> {
        interrupt.checked()?;
        // The copy came from an admitted store, so this ordinary open settles
        // its journal mode and initializes nothing. Closing it folds its log;
        // that last checkpoint is checked only after it returns.
        let connection = Connection::open(path)?;
        interrupt.install(&connection, CopyProbePoint::Settle)?;
        drop(Self::from_connection_with_busy_timeout(
            connection,
            None,
            None,
            interrupt.lock_wait(),
        )?);
        for sidecar in store_sidecars(path) {
            match std::fs::remove_file(&sidecar) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(StoreError::InvalidWork(format!(
                        "cannot remove {}: {error}",
                        sidecar.display()
                    )));
                }
            }
        }
        interrupt.checked()?;
        let (manifest, store) = Self::verify_copy_file(path, Some(interrupt))?;
        interrupt.checked()?;
        let schema_reference =
            CanonicalObject::freeze(&super::super::stored_schema_definitions(&store.connection)?)?
                .key()
                .clone();
        let cut = WorkGraphSnapshotCut {
            work_feed: store.work_feed_head(&FeedId::Project(project.clone()))?,
            project_memory: super::super::project_memory::project_memory_state_on(
                &store.connection,
                project,
            )?
            .1,
        };
        drop(store);
        interrupt.checked()?;
        Ok(VerifiedStoreCopy {
            file_sha256: manifest.file_sha256,
            file_bytes: manifest.file_bytes,
            schema_reference,
            cut,
        })
    }
}
