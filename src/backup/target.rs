//! A project's backup targets: the operator's configuration and the recorded
//! state for each copy kind, kept as files under the Engram home, and the
//! push lock that serializes every write to them.
//!
//! These records are host-local operational files. They hold no secret, are
//! not canonical work state, are part of no copy, and never authorize a
//! restore. A clean home has no target and no state.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

use super::CopyKind;
use crate::{CanonicalObject, ObjectId, ProjectId};

/// The directory below the Engram home that holds every project's records.
pub const RECORDS_DIRECTORY: &str = "backup-records";

/// The record format this build writes and reads; any other is refused.
pub const RECORD_FORMAT_VERSION: u32 = 1;

/// How long a copy keeps a kind qualified when no window is given, in hours.
pub const DEFAULT_WINDOW_HOURS: u32 = 24;

/// How many copies a target keeps when no count is given.
pub const DEFAULT_KEEP: u32 = 3;

/// How a target is reached.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterKind {
    /// An absolute path on this host: a share, a removable disk or a folder
    /// a sync client replicates.
    Directory,
}

impl AdapterKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Directory => "directory",
        }
    }
}

impl CopyKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Store => "store",
        }
    }

    /// Every kind this build can configure.
    pub const ALL: [Self; 1] = [Self::Store];
}

/// One statement by the operator, recorded as asserted context: who said it
/// and when. Engram records it and never makes it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Statement {
    pub by: String,
    pub at: DateTime<Utc>,
}

/// One configured target for one project and kind. Its identity is not
/// stored: it is derived from these fields whenever it is needed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    pub format_version: u32,
    pub project: String,
    pub kind: CopyKind,
    pub adapter: AdapterKind,
    /// The absolute directory, as the operator spelled it.
    pub dir: String,
    pub window_hours: u32,
    pub keep: u32,
    /// The destination may hold everything the kind carries.
    pub disclosure_authorized: Statement,
    /// The destination leaves the machine; required for a directory.
    pub off_host_asserted: Option<Statement>,
}

impl TargetConfig {
    /// The target's identity, derived from the project, kind, adapter,
    /// location and both statements.
    ///
    /// # Errors
    ///
    /// Returns [`TargetError::Identity`] when the input cannot be canonicalized.
    pub fn identity(&self) -> Result<ObjectId, TargetError> {
        target_identity(&IdentityInput {
            project: &self.project,
            kind: self.kind.as_str(),
            adapter: self.adapter.as_str(),
            location: &self.dir,
            disclosure_authorized: &self.disclosure_authorized,
            off_host_asserted: self.off_host_asserted.as_ref(),
        })
    }
}

/// What this host recorded about a kind's copies. Pushes fill it in; a newly
/// set target starts from this empty state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetState {
    pub format_version: u32,
    /// The identity of the target this state was started for, compared with
    /// the configured target's to tell whether the state belongs to it.
    pub target_identity: ObjectId,
}

/// What `target set` is asked to record.
#[derive(Clone, Debug)]
pub struct TargetRequest {
    pub kind: CopyKind,
    pub adapter: AdapterKind,
    pub dir: PathBuf,
    pub disclosure_authorized_by: String,
    pub off_host_asserted_by: Option<String>,
    pub window_hours: u32,
    pub keep: u32,
}

/// A configured kind as the target words report it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TargetView {
    pub config: TargetConfig,
    pub identity: ObjectId,
    /// Whether this target's state file exists.
    pub state_recorded: bool,
}

/// Why a target word refused or failed. Each case has a stable code.
#[derive(Debug, Error)]
pub enum TargetError {
    #[error("{reason}")]
    Invalid { reason: String },
    #[error("{} cannot be used: {reason}", path.display())]
    Unreadable { path: PathBuf, reason: String },
    #[error("another process holds the push lock {}; try again when it ends", path.display())]
    PushRunning { path: PathBuf },
    #[error("{} could not be read or written: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "{} was written but {} could not be: {source}; run `engram backup target set` again",
        written.display(),
        path.display()
    )]
    Partial {
        written: PathBuf,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("the backup target identity could not be derived: {0}")]
    Identity(String),
}

impl TargetError {
    /// The stable code of this failure.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Invalid { .. } => "backup_target_invalid",
            Self::Unreadable { .. } => "backup_record_unreadable",
            Self::PushRunning { .. } => "backup_push_running",
            Self::Io { .. } | Self::Identity(_) => "backup_io",
            Self::Partial { .. } => "backup_record_partial",
        }
    }
}

/// The files that hold one project's records for one kind.
#[derive(Clone, Debug)]
pub struct RecordPaths {
    pub directory: PathBuf,
    pub config: PathBuf,
    pub state: PathBuf,
    pub lock: PathBuf,
}

impl RecordPaths {
    #[must_use]
    pub fn new(home: &Path, project: &ProjectId, kind: CopyKind) -> Self {
        let directory = home
            .join(RECORDS_DIRECTORY)
            .join(crate::project_digest(project));
        let name = kind.as_str();
        Self {
            config: directory.join(format!("{name}.target.json")),
            state: directory.join(format!("{name}.state.json")),
            lock: directory.join(format!("{name}.lock")),
            directory,
        }
    }
}

/// The exclusive push lock for one project and kind. The operating system
/// holds it for the process and releases it when the guard is dropped or the
/// process ends; there is no takeover by age. The lock file itself is never
/// removed, so every process locks the same file.
#[derive(Debug)]
pub struct PushLock {
    _file: File,
}

impl PushLock {
    /// Takes the lock without waiting.
    ///
    /// # Errors
    ///
    /// [`TargetError::PushRunning`] when another holder has it, and
    /// [`TargetError::Io`] when the lock file cannot be opened or locked.
    pub fn try_acquire(paths: &RecordPaths) -> Result<Self, TargetError> {
        fs::create_dir_all(&paths.directory).map_err(|source| TargetError::Io {
            path: paths.directory.clone(),
            source,
        })?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&paths.lock)
            .map_err(|source| TargetError::Io {
                path: paths.lock.clone(),
                source,
            })?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(fs::TryLockError::WouldBlock) => Err(TargetError::PushRunning {
                path: paths.lock.clone(),
            }),
            Err(fs::TryLockError::Error(source)) => Err(TargetError::Io {
                path: paths.lock.clone(),
                source,
            }),
        }
    }
}

/// What a target's identity is derived from. Changing any part gives a new
/// identity, which ends the qualification of receipts issued for the old one.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct IdentityInput<'a> {
    pub project: &'a str,
    pub kind: &'a str,
    pub adapter: &'a str,
    pub location: &'a str,
    pub disclosure_authorized: &'a Statement,
    pub off_host_asserted: Option<&'a Statement>,
}

/// The content fingerprint that names a target, compared to tell whether a
/// target changed.
///
/// # Errors
///
/// Returns [`TargetError::Identity`] when the input cannot be canonicalized.
pub fn target_identity(input: &IdentityInput<'_>) -> Result<ObjectId, TargetError> {
    CanonicalObject::freeze(input)
        .map(|object| object.key().clone())
        .map_err(|error| TargetError::Identity(error.to_string()))
}

/// Records a target for `project`, replacing any earlier one for its kind,
/// and starts its state anew. This is the way on from a record this build
/// cannot read: both files for the kind are written in the current format.
///
/// # Errors
///
/// Refuses an invalid request, a held push lock, or a failed write.
pub fn set_target(
    home: &Path,
    project: &ProjectId,
    request: &TargetRequest,
    now: DateTime<Utc>,
) -> Result<TargetView, TargetError> {
    let dir = request
        .dir
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| TargetError::Invalid {
            reason: "--dir must be valid Unicode".into(),
        })?;
    let config = TargetConfig {
        format_version: RECORD_FORMAT_VERSION,
        project: project.0.clone(),
        kind: request.kind,
        adapter: request.adapter,
        dir,
        window_hours: request.window_hours,
        keep: request.keep,
        disclosure_authorized: Statement {
            by: request.disclosure_authorized_by.clone(),
            at: now,
        },
        off_host_asserted: request.off_host_asserted_by.as_ref().map(|by| Statement {
            by: by.clone(),
            at: now,
        }),
    };
    config_problem(&config, project, request.kind)
        .map_or(Ok(()), |reason| Err(TargetError::Invalid { reason }))?;
    let identity = config.identity()?;
    let state = TargetState {
        format_version: RECORD_FORMAT_VERSION,
        target_identity: identity.clone(),
    };
    let paths = RecordPaths::new(home, project, request.kind);
    let _lock = PushLock::try_acquire(&paths)?;
    // The state goes first, so a new configuration never stands beside the
    // state of the target it replaced; the opposite pairing is refused when
    // read.
    write_record(&paths.state, &state).map_err(|source| TargetError::Io {
        path: paths.state.clone(),
        source,
    })?;
    write_record(&paths.config, &config).map_err(|source| TargetError::Partial {
        written: paths.state.clone(),
        path: paths.config.clone(),
        source,
    })?;
    Ok(TargetView {
        config,
        identity,
        state_recorded: true,
    })
}

/// The configured targets of `project`, one per configured kind.
///
/// # Errors
///
/// Refuses with [`TargetError::Unreadable`] when a configuration or state file
/// exists but this build cannot use it; nothing is changed.
pub fn show_targets(home: &Path, project: &ProjectId) -> Result<Vec<TargetView>, TargetError> {
    let mut views = Vec::new();
    for kind in CopyKind::ALL {
        let paths = RecordPaths::new(home, project, kind);
        let (config, state) = read_records(&paths, project, kind)?;
        if let Some((config, identity)) = config {
            views.push(TargetView {
                config,
                identity,
                state_recorded: state.is_some(),
            });
        }
    }
    Ok(views)
}

/// Removes the target and state of `kind` for `project`. The lock file stays.
/// Returns whether a target was configured.
///
/// # Errors
///
/// Refuses a held push lock, a record this build cannot use (nothing is then
/// removed), or a failed removal.
pub fn clear_target(home: &Path, project: &ProjectId, kind: CopyKind) -> Result<bool, TargetError> {
    let paths = RecordPaths::new(home, project, kind);
    // A first read refuses a record that exists but cannot be read or used,
    // whatever stands in the way of reading it. Only a missing file counts as
    // absent. A kind with no records and no lock file has nothing to clear
    // and nobody holding its lock, so nothing is created for it; otherwise
    // the lock is taken, and a holder makes clear refuse even with no records.
    let (config, state) = read_records(&paths, project, kind)?;
    if config.is_none() && state.is_none() {
        match fs::metadata(&paths.lock) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(source) => {
                return Err(TargetError::Io {
                    path: paths.lock.clone(),
                    source,
                });
            }
            Ok(_) => {}
        }
    }
    let _lock = PushLock::try_acquire(&paths)?;
    let (config, _state) = read_records(&paths, project, kind)?;
    for path in [&paths.config, &paths.state] {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(TargetError::Io {
                    path: path.clone(),
                    source,
                });
            }
        }
    }
    Ok(config.is_some())
}

/// A kind's usable configuration with its derived identity, and its state.
type Records = (Option<(TargetConfig, ObjectId)>, Option<TargetState>);

/// Reads both records of one kind, refusing either when it exists but cannot
/// be used: a configuration `set` would have refused, or a state recorded
/// for another target than the configured one.
fn read_records(
    paths: &RecordPaths,
    project: &ProjectId,
    kind: CopyKind,
) -> Result<Records, TargetError> {
    let config = read_record::<TargetConfig>(&paths.config)?;
    let state = read_record::<TargetState>(&paths.state)?;
    let Some(config) = config else {
        return Ok((None, state));
    };
    if let Some(reason) = config_problem(&config, project, kind) {
        return Err(TargetError::Unreadable {
            path: paths.config.clone(),
            reason: format!("{reason}; `engram backup target set` writes it anew"),
        });
    }
    let identity = config.identity()?;
    if let Some(state) = &state
        && state.target_identity != identity
    {
        return Err(TargetError::Unreadable {
            path: paths.state.clone(),
            reason: "it was recorded for another target than the configured one; `engram backup target set` writes both anew".into(),
        });
    }
    Ok((Some((config, identity)), state))
}

/// What makes a configuration one `set` refuses to record, if anything. The
/// same rules decide whether a stored one can be used.
fn config_problem(config: &TargetConfig, project: &ProjectId, kind: CopyKind) -> Option<String> {
    if config.format_version != RECORD_FORMAT_VERSION {
        return Some(format!(
            "its format version {} is not one this build knows",
            config.format_version
        ));
    }
    if config.project != project.0 {
        return Some("it names another project".into());
    }
    if config.kind != kind {
        return Some("it names another kind".into());
    }
    if !Path::new(&config.dir).is_absolute() {
        return Some(format!(
            "--dir must be an absolute path, not {}",
            config.dir
        ));
    }
    if let Some(problem) = statement_problem(
        "--disclosure-authorized-by",
        &config.disclosure_authorized.by,
    ) {
        return Some(problem);
    }
    match (&config.off_host_asserted, config.adapter) {
        (Some(statement), _) => {
            if let Some(problem) = statement_problem("--off-host-asserted-by", &statement.by) {
                return Some(problem);
            }
        }
        (None, AdapterKind::Directory) => {
            return Some(
                "a directory target needs --off-host-asserted-by: the operator's statement that the directory leaves this machine".into(),
            );
        }
    }
    if config.window_hours == 0 {
        return Some("--window-hours must be at least 1".into());
    }
    if config.keep == 0 {
        return Some("--keep must be at least 1".into());
    }
    None
}

fn statement_problem(flag: &str, name: &str) -> Option<String> {
    (name.trim().is_empty() || name.chars().any(char::is_control))
        .then(|| format!("{flag} must name the operator, without control characters"))
}

fn read_record<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, TargetError> {
    let unreadable = |reason: String| TargetError::Unreadable {
        path: path.to_path_buf(),
        reason,
    };
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(unreadable(format!("it cannot be read: {error}"))),
    };
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| unreadable(format!("it is not JSON: {error}")))?;
    match value
        .get("format_version")
        .and_then(serde_json::Value::as_u64)
    {
        Some(version) if version == u64::from(RECORD_FORMAT_VERSION) => {}
        Some(version) => {
            return Err(unreadable(format!(
                "its format version {version} is not one this build knows; `engram backup target set` writes it anew"
            )));
        }
        None => return Err(unreadable("it has no format version".into())),
    }
    serde_json::from_value(value).map(Some).map_err(|error| {
        unreadable(format!(
            "its fields do not match its format version: {error}"
        ))
    })
}

/// Replaces `path` whole: a new sibling file is written, synced and closed,
/// then renamed over it.
fn write_record<T: Serialize>(path: &Path, record: &T) -> io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(record).map_err(io::Error::other)?;
    bytes.push(b'\n');
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("a record path has no file name"))?;
    let mut temporary = name.to_owned();
    temporary.push(format!(".{}.tmp", uuid::Uuid::now_v7()));
    let temporary = path.with_file_name(temporary);
    let written = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written
}

#[cfg(test)]
mod tests;
