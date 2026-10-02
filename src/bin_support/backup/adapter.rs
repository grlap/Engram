//! The `BackupAdapter` port: the four requests through which a copy reaches a
//! target and comes back. The core produces the artifact and its manifest;
//! an adapter only moves bytes and reports what the target confirmed.

use std::{io, path::PathBuf, time::Duration};

use engram::ProjectId;
pub(crate) use engram::backup::record::{
    Acknowledgement, Attempt, BackupReceipt, Encoding, OffHost, STORED_FORMAT_VERSION,
    StoredManifest,
};
use thiserror::Error;

/// Whether a target holds exactly one copy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Confirmation {
    /// The stored file was read in full and decodes to the manifest's bytes.
    Confirmed,
    /// The copy is not there, or other bytes stand under its name.
    Missing { reason: String },
    /// The configured location cannot be reached at all.
    Unreachable { reason: String },
    /// The target was reached, and the read of the copy passed its deadline
    /// before it could say; nothing against the copy.
    TimedOut { reason: String },
    /// The target was reached but could not be read well enough to say.
    Unknown { reason: String },
}

impl Confirmation {
    /// Why the target could not say, when it could not.
    pub(crate) fn undecided(&self) -> Option<&str> {
        match self {
            Self::Unreachable { reason } | Self::TimedOut { reason } | Self::Unknown { reason } => {
                Some(reason)
            }
            Self::Confirmed | Self::Missing { .. } => None,
        }
    }
}

/// One page of a project's copies at a target.
#[derive(Clone, Debug, Default)]
pub(crate) struct ManifestPage {
    pub manifests: Vec<StoredManifest>,
    /// Names of manifest files that could not be used, left untouched.
    pub unreadable: Vec<String>,
    /// Where the next page starts, if there is one.
    pub next: Option<String>,
}

/// What reconciling a recorded attempt found and did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Reconciled {
    /// The copy and its manifest are both in place.
    Complete,
    /// The data file was in place with matching content; its manifest was
    /// written now.
    Completed,
    /// The attempt's files held other content or were partial; they were
    /// removed.
    Removed,
    /// Nothing of the attempt is at the target.
    Absent,
    /// The target could not be read well enough to say; nothing was changed.
    Unknown { reason: String },
}

/// Why a request failed. Each case has a stable code.
#[derive(Debug, Error)]
pub(crate) enum AdapterError {
    #[error("the target {} has {available} bytes free; the copy needs {required}", path.display())]
    TargetNoSpace {
        path: PathBuf,
        required: u64,
        available: u64,
    },
    #[error("the free space of the target {} could not be read: {source}", path.display())]
    TargetSpaceUnknown {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("the target {} cannot be reached: {reason}", path.display())]
    Unreachable { path: PathBuf, reason: String },
    #[error("the target {} filled up while the copy was written: {source}", path.display())]
    TargetFull {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("the free space of the local disk at {} could not be read: {source}", path.display())]
    LocalSpaceUnknown {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("the local disk at {} has {available} bytes free; the copy needs {required}", path.display())]
    LocalNoSpace {
        path: PathBuf,
        required: u64,
        available: u64,
    },
    #[error("{} is not at the target", path.display())]
    CopyMissing { path: PathBuf },
    #[error("{error}; {} could not be removed after it: {source}", path.display())]
    Leftover {
        error: Box<AdapterError>,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{} already exists and is never replaced", path.display())]
    Exists { path: PathBuf },
    #[error("{}: {reason}", path.display())]
    CopyInvalid { path: PathBuf, reason: String },
    #[error("{} took longer than its deadline of {deadline:?}", path.display())]
    Deadline { path: PathBuf, deadline: Duration },
    #[error("preparing {} took longer than the capture's deadline", path.display())]
    PrepareDeadline { path: PathBuf },
    #[error("{} could not be read or written: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl AdapterError {
    /// The stable code of this failure.
    #[must_use]
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::CopyMissing { .. } => "backup_copy_missing",
            Self::Leftover { error, .. } => error.code(),
            Self::TargetNoSpace { .. } | Self::TargetFull { .. } => "backup_target_no_space",
            Self::Unreachable { .. } => "backup_target_unreachable",
            Self::LocalSpaceUnknown { .. } => "backup_local_space_unknown",
            Self::TargetSpaceUnknown { .. } => "backup_target_space_unknown",
            Self::LocalNoSpace { .. } => "backup_local_no_space",
            Self::Exists { .. } => "backup_copy_exists",
            Self::CopyInvalid { .. } => "backup_copy_invalid",
            Self::Deadline { .. } => "backup_transport_deadline",
            Self::PrepareDeadline { .. } => "backup_capture_deadline",
            Self::Io { .. } => "backup_io",
        }
    }
}

/// The four requests of the port, as spec §9.2 names them.
pub(crate) trait BackupAdapter {
    /// Stores one immutable copy of `artifact` and answers with a receipt.
    fn put(
        &self,
        project: &ProjectId,
        attempt: &Attempt,
        artifact: &std::path::Path,
    ) -> Result<BackupReceipt, AdapterError>;

    /// Says whether the target holds exactly the copy `manifest` describes.
    fn confirm(&self, project: &ProjectId, manifest: &StoredManifest) -> Confirmation;

    /// The manifests of the copies the target holds for `project`, from
    /// `cursor` on.
    fn list(&self, project: &ProjectId, cursor: Option<&str>)
    -> Result<ManifestPage, AdapterError>;

    /// Writes one copy, decoded and checked, to `destination`, which must not
    /// exist yet.
    fn get(
        &self,
        project: &ProjectId,
        manifest: &StoredManifest,
        destination: &std::path::Path,
    ) -> Result<(), AdapterError>;
}
