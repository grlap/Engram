//! Off-host backup: the capture of a verified store copy into a local stage
//! under the Engram home, with the manifest that describes it.
//!
//! A capture never writes the live store: it copies through a read-only
//! connection, then settles, checks and fingerprints the staged file. What
//! later moves the copy to a target, and the record of what a target
//! confirmed, are separate steps.

use std::{
    fmt, io,
    path::{Path, PathBuf},
    time::Duration,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    ObjectId, ProjectId, SqliteStore, StoreError, WorkGraphSnapshotCut, storage::CopyInterrupt,
};

/// The directory below the Engram home that holds every capture stage.
pub const STAGE_DIRECTORY: &str = "backup-stage";

/// The file name of a staged store copy inside its attempt directory.
const STAGED_STORE: &str = "store.db";

/// What a copy carries; each kind is captured, confirmed and restored alone.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyKind {
    /// The verified full-store file.
    Store,
}

/// Describes one captured copy. Every identity is its own field: the store
/// format, the capturing build and the source revision are never merged.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureManifest {
    /// The opaque name of the project's directories below the Engram home.
    pub project_digest: String,
    pub kind: CopyKind,
    /// The project work-feed head and project-memory change position, read
    /// from the finished copy, not from the live store.
    pub cut: WorkGraphSnapshotCut,
    /// Taken before the copy began.
    pub capture_started_at: DateTime<Utc>,
    pub bytes: u64,
    /// SHA-256 of the copy's bytes, as a content fingerprint.
    pub sha256: String,
    /// The schema reference of the copy: the store format it restores into.
    pub format_identity: ObjectId,
    /// The fingerprint of the build that captured and checked the copy. Each
    /// optional field must be present, as null when empty.
    #[serde(deserialize_with = "Option::deserialize")]
    pub build_fingerprint: Option<ObjectId>,
    /// The source revision of that build: a commit id, that id followed by
    /// `+dirty`, or `unavailable` when the build could not determine it.
    #[serde(deserialize_with = "Option::deserialize")]
    pub source_revision: Option<String>,
    /// The capturing host's name, as asserted context.
    #[serde(deserialize_with = "Option::deserialize")]
    pub host_name: Option<String>,
}

/// The step a capture was in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapturePhase {
    /// Checking the stage's free space.
    Space,
    /// Copying the live store into the stage.
    Copy,
    /// Settling and checking the staged copy and reading its cut.
    Verify,
}

impl CapturePhase {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Space => "space",
            Self::Copy => "copy",
            Self::Verify => "verify",
        }
    }
}

impl fmt::Display for CapturePhase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Why a capture failed. Each case has a stable code.
#[derive(Debug, Error)]
pub enum BackupError {
    #[error(
        "the backup stage {} has {available} bytes free; the capture needs {required}",
        stage.display()
    )]
    StageNoSpace {
        stage: PathBuf,
        required: u64,
        available: u64,
    },
    #[error("the free space of the backup stage {} could not be read: {source}", stage.display())]
    StageSpaceUnknown {
        stage: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("the capture passed its deadline during the {phase} step")]
    CaptureDeadline { phase: CapturePhase },
    #[error("{} could not be read or written: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("{error}; its stage {} could not be removed: {source}", path.display())]
    StageCleanup {
        error: Box<BackupError>,
        path: PathBuf,
        source: io::Error,
    },
}

impl BackupError {
    /// The stable code of this failure.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::StageNoSpace { .. } => "backup_stage_no_space",
            Self::StageSpaceUnknown { .. } => "backup_stage_space_unknown",
            Self::CaptureDeadline { .. } => "backup_capture_deadline",
            Self::Io { .. } => "backup_io",
            Self::Store(error) => crate::host::store_error_code(error),
            Self::StageCleanup { error, .. } => error.code(),
        }
    }
}

/// How a capture runs.
pub struct CaptureOptions<'a> {
    /// The whole capture's time limit, from its start.
    pub deadline: Duration,
    /// Whether a compressed file will be built in the stage beside the copy,
    /// which needs as much room again.
    pub compressed_in_stage: bool,
    /// The capturing host's name, recorded as asserted context.
    pub host_name: Option<String>,
    /// Reads the free space of the file system holding a path.
    pub free_space: &'a dyn Fn(&Path) -> io::Result<u64>,
    /// Told when each step begins.
    pub observer: Option<&'a dyn Fn(CapturePhase)>,
    #[cfg(test)]
    pub(crate) copy_probe:
        Option<std::sync::Arc<dyn Fn(crate::storage::CopyProbePoint) -> bool + Send + Sync>>,
}

impl CaptureOptions<'static> {
    /// Options that read the real free space and this host's name.
    #[must_use]
    pub fn new(deadline: Duration, compressed_in_stage: bool) -> Self {
        Self {
            deadline,
            compressed_in_stage,
            host_name: host_name(),
            free_space: &available_space,
            observer: None,
            #[cfg(test)]
            copy_probe: None,
        }
    }
}

/// The free space available to this user on the file system holding `path`.
///
/// # Errors
///
/// Returns the operating system's error when the reading fails.
pub fn available_space(path: &Path) -> io::Result<u64> {
    fs4::available_space(path)
}

/// This host's name from the environment, as asserted context.
#[must_use]
pub fn host_name() -> Option<String> {
    ["COMPUTERNAME", "HOSTNAME"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok())
        .filter(|name| !name.trim().is_empty())
}

/// A captured copy waiting in its own stage directory. The stage is the
/// caller's until [`StoreCapture::discard`] removes it.
#[derive(Debug)]
pub struct StoreCapture {
    pub manifest: CaptureManifest,
    /// The staged copy file.
    pub staged: PathBuf,
    attempt: PathBuf,
}

impl StoreCapture {
    /// The directory this capture owns.
    #[must_use]
    pub fn stage(&self) -> &Path {
        &self.attempt
    }

    /// Removes the staged copy and its stage directory.
    ///
    /// # Errors
    ///
    /// Returns the first removal that failed; every owned path is attempted.
    pub fn discard(self) -> Result<(), BackupError> {
        remove_attempt(&self.attempt).map_err(|(path, source)| BackupError::Io { path, source })
    }
}

/// Captures a verified copy of `project`'s store under `home` into a new stage
/// directory below `<home>/backup-stage/<project digest>/`.
///
/// Before anything is written, the stage's file system must have room for the
/// store file and its log, and as much again when `compressed_in_stage` is
/// set. The capture start time is taken before the copy begins. A failed
/// capture removes its own stage directory and nothing else.
///
/// # Errors
///
/// Returns [`BackupError`] with a stable code: the store is refused, the stage
/// lacks room or its free space cannot be read, the deadline passes, or a copy
/// step fails.
pub fn capture_store(
    home: &Path,
    project: &ProjectId,
    options: &CaptureOptions<'_>,
) -> Result<StoreCapture, BackupError> {
    let interrupt = CopyInterrupt::after(options.deadline);
    #[cfg(test)]
    let interrupt = match &options.copy_probe {
        Some(probe) => interrupt.with_probe(probe.clone()),
        None => interrupt,
    };
    // Read within the capture's time: its first reading in a process hashes
    // the running executable.
    let build_fingerprint = crate::build_identity::current().build_fingerprint.clone();
    let observe = |phase| {
        if let Some(observer) = options.observer {
            observer(phase);
        }
    };
    let database = crate::project_database_path(home, project);
    let digest = crate::project_digest(project);
    let stage_root = home.join(STAGE_DIRECTORY).join(&digest);

    observe(CapturePhase::Space);
    let database_bytes = match std::fs::metadata(&database) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(StoreError::StoreNotInitialized.into());
        }
        Err(source) => {
            return Err(BackupError::Io {
                path: database,
                source,
            });
        }
    };
    let log = sidecar(&database, "-wal");
    let log_bytes = match std::fs::metadata(&log) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
        Err(source) => return Err(BackupError::Io { path: log, source }),
    };
    // Saturating: a size past u64 needs more room than any file system has.
    let copy_bytes = database_bytes.saturating_add(log_bytes);
    let required = if options.compressed_in_stage {
        copy_bytes.saturating_mul(2)
    } else {
        copy_bytes
    };
    // The stage may not exist yet; its nearest existing ancestor is on the
    // same file system, and reading it creates nothing.
    let probe = nearest_existing_ancestor(&stage_root);
    let available =
        (options.free_space)(&probe).map_err(|source| BackupError::StageSpaceUnknown {
            stage: stage_root.clone(),
            source,
        })?;
    if available < required {
        return Err(BackupError::StageNoSpace {
            stage: stage_root,
            required,
            available,
        });
    }
    if interrupt.expired() {
        return Err(BackupError::CaptureDeadline {
            phase: CapturePhase::Space,
        });
    }

    std::fs::create_dir_all(&stage_root).map_err(|source| BackupError::Io {
        path: stage_root.clone(),
        source,
    })?;
    // An attempt directory created here, and never before, is what this
    // capture owns and may remove.
    let attempt = stage_root.join(uuid::Uuid::now_v7().to_string());
    std::fs::create_dir(&attempt).map_err(|source| BackupError::Io {
        path: attempt.clone(),
        source,
    })?;
    let staged = attempt.join(STAGED_STORE);
    let captured = capture_into(
        &database,
        &staged,
        project,
        CaptureIdentity {
            digest: &digest,
            build_fingerprint,
        },
        options,
        &interrupt,
        &observe,
    );
    match captured {
        Ok(manifest) => Ok(StoreCapture {
            manifest,
            staged,
            attempt,
        }),
        Err(error) => match remove_attempt(&attempt) {
            Ok(()) => Err(error),
            Err((path, source)) => Err(BackupError::StageCleanup {
                error: Box::new(error),
                path,
                source,
            }),
        },
    }
}

/// The identities a capture fixes before it copies anything.
struct CaptureIdentity<'a> {
    digest: &'a str,
    build_fingerprint: Option<ObjectId>,
}

fn capture_into(
    database: &Path,
    staged: &Path,
    project: &ProjectId,
    identity: CaptureIdentity<'_>,
    options: &CaptureOptions<'_>,
    interrupt: &CopyInterrupt,
    observe: &dyn Fn(CapturePhase),
) -> Result<CaptureManifest, BackupError> {
    let classified = |phase: CapturePhase| {
        move |error: StoreError| {
            // A failure once the deadline has passed is the deadline's, even
            // when it surfaced as another error, such as a lock wait that
            // the time left cut short.
            if interrupt.expired() || is_interrupted(&error) {
                BackupError::CaptureDeadline { phase }
            } else {
                BackupError::Store(error)
            }
        }
    };
    observe(CapturePhase::Copy);
    let capture_started_at = Utc::now();
    SqliteStore::copy_existing_read_only(database, staged, interrupt)
        .map_err(classified(CapturePhase::Copy))?;
    observe(CapturePhase::Verify);
    let copy = SqliteStore::verify_store_copy(staged, project, interrupt)
        .map_err(classified(CapturePhase::Verify))?;
    if interrupt.expired() {
        return Err(BackupError::CaptureDeadline {
            phase: CapturePhase::Verify,
        });
    }
    Ok(CaptureManifest {
        project_digest: identity.digest.to_owned(),
        kind: CopyKind::Store,
        cut: copy.cut,
        capture_started_at,
        bytes: copy.file_bytes,
        sha256: copy.file_sha256,
        format_identity: copy.schema_reference,
        build_fingerprint: identity.build_fingerprint,
        source_revision: Some(crate::build_identity::source_revision().to_owned()),
        host_name: options.host_name.clone(),
    })
}

fn is_interrupted(error: &StoreError) -> bool {
    matches!(
        error,
        StoreError::Sqlite(rusqlite::Error::SqliteFailure(failure, _))
            if failure.code == rusqlite::ErrorCode::OperationInterrupted
    )
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn nearest_existing_ancestor(path: &Path) -> PathBuf {
    path.ancestors()
        .find(|ancestor| ancestor.exists())
        .unwrap_or(path)
        .to_path_buf()
}

/// Removes the files a capture can leave in its attempt directory, then the
/// directory itself. Every path is attempted; a missing one is fine. The
/// directory removal is not recursive, so a file nobody here wrote stays.
fn remove_attempt(attempt: &Path) -> Result<(), (PathBuf, io::Error)> {
    let staged = attempt.join(STAGED_STORE);
    let mut first_failure = None;
    let sidecars = crate::storage::store_sidecars(&staged);
    for path in std::iter::once(staged).chain(sidecars) {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                first_failure.get_or_insert((path, error));
            }
        }
    }
    if let Some(failure) = first_failure {
        return Err(failure);
    }
    match std::fs::remove_dir(attempt) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err((attempt.to_path_buf(), error)),
    }
}

pub mod freshness;
pub mod record;
pub mod reminder;
pub mod restore;
pub mod status;
pub mod target;

#[cfg(test)]
mod tests;
