//! The provenance record `engram backup restore` leaves under the Engram home:
//! which copy it installed, that copy's SHA-256 and origin host, and the
//! operator's statement that the origin store will never run again. It is
//! written pending before the store is moved into place and marked completed
//! after the installed store is checked, so a restore that stopped between the
//! two stays visible.
//!
//! The record is host-local and asserted context. It is no backup receipt and
//! qualifies no copy; `backup target clear` leaves it in place. `backup status`
//! and `doctor` read it without contacting any target.
//!
//! A pending restore the operator abandons is archived whole, beside the
//! active record, in an envelope that names who abandoned it and when, and the
//! active record is then cleared.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{
    CopyKind,
    target::{PushLock, RECORD_FORMAT_VERSION, RecordPaths, Statement, TargetError},
};
use crate::ProjectId;

/// Where a restore stands.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreState {
    /// Recorded before the move; the store may not be in place.
    Pending,
    /// The store was moved into place and checked.
    Completed,
}

/// One restore of a `store` copy into this home.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreRecord {
    pub format_version: u32,
    pub project: String,
    pub copy: String,
    /// The SHA-256 of the copy's bytes, which the installed store keeps.
    pub sha256: String,
    /// The host that captured the copy, as its manifest names it.
    pub origin_host: Option<String>,
    /// The operator's statement that the origin store will never run again.
    pub origin_retired: Statement,
    /// The fetched copy, beside the store, until it is moved into place.
    pub staging: String,
    pub state: RestoreState,
    pub pending_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

/// An abandoned pending restore, as its archive keeps it: who abandoned it and
/// when, and the pending record as it was parsed, every field kept.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AbandonedRestore {
    pub format_version: u32,
    pub abandoned: Statement,
    pub pending: serde_json::Value,
}

/// An archived abandoned restore, and what the operator should know about it.
#[derive(Debug)]
pub struct Archived {
    pub archive: PathBuf,
    /// A temporary file the move left behind that could not be removed.
    pub warnings: Vec<String>,
}

/// Why archiving an abandoned pending restore stopped.
#[derive(Debug)]
pub enum AbandonError {
    /// No archive was written; the pending record stays active.
    Archive(TargetError),
    /// The archive was written, but the active record could not be cleared,
    /// so it stays pending.
    Clear {
        archive: PathBuf,
        error: TargetError,
    },
}

/// What the home records about a restore.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RestoreRecords {
    None,
    Recorded(Box<RestoreRecord>),
    Unreadable { path: PathBuf, reason: String },
}

/// The file that holds the project's restore record.
#[must_use]
pub fn restore_record_path(home: &Path, project: &ProjectId) -> PathBuf {
    RecordPaths::new(home, project, CopyKind::Store)
        .directory
        .join(super::target::RESTORE_RECORD)
}

/// Reads the project's restore record without taking a lock. A record that
/// exists but cannot be used is reported as unreadable, never as an error.
#[must_use]
pub fn read_restore_record(home: &Path, project: &ProjectId) -> RestoreRecords {
    let path = restore_record_path(home, project);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return RestoreRecords::None,
        Err(error) => {
            return RestoreRecords::Unreadable {
                path,
                reason: format!("it cannot be read: {error}"),
            };
        }
    };
    match parse(&bytes, project) {
        Ok(record) => RestoreRecords::Recorded(Box::new(record)),
        Err(reason) => RestoreRecords::Unreadable { path, reason },
    }
}

fn parse(bytes: &[u8], project: &ProjectId) -> Result<RestoreRecord, String> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|error| format!("it is not JSON: {error}"))?;
    match value
        .get("format_version")
        .and_then(serde_json::Value::as_u64)
    {
        Some(version) if version == u64::from(RECORD_FORMAT_VERSION) => {}
        Some(version) => {
            return Err(format!(
                "its format version {version} is not one this build knows"
            ));
        }
        None => return Err("it has no format version".into()),
    }
    let record: RestoreRecord = serde_json::from_value(value)
        .map_err(|error| format!("its fields do not match its format version: {error}"))?;
    if record.project != project.0 {
        return Err("it names another project".into());
    }
    match (record.state, record.completed_at) {
        (RestoreState::Pending, None) | (RestoreState::Completed, Some(_)) => Ok(record),
        (RestoreState::Pending, Some(_)) => Err("a pending restore names a completion time".into()),
        (RestoreState::Completed, None) => {
            Err("a completed restore names no completion time".into())
        }
    }
}

/// Replaces the project's restore record whole, for a restore that holds the
/// push lock: a new file is written and renamed into place.
///
/// # Errors
///
/// Refuses a lock taken for another kind or project, and reports a failed
/// write; the earlier record then stays as it was.
pub fn write_restore_record(
    home: &Path,
    project: &ProjectId,
    lock: &PushLock,
    record: &RestoreRecord,
) -> Result<(), TargetError> {
    let paths = RecordPaths::new(home, project, CopyKind::Store);
    super::target::held(&paths, lock)?;
    let path = restore_record_path(home, project);
    super::target::write_record(&path, record).map_err(|source| TargetError::Io { path, source })
}

/// Keeps an earlier completed record under a name stamped with its completion
/// time in UTC, `store.restore-<YYYYMMDDTHHMMSSZ>.json`, before a new restore
/// writes its own; an existing kept record is never replaced, so a second one
/// in the same second takes the next free `-N` suffix. Returns where it was
/// kept.
///
/// # Errors
///
/// Refuses a lock taken for another kind or project, a record that is not a
/// completed one, and a failed move.
pub fn keep_completed_record(
    home: &Path,
    project: &ProjectId,
    lock: &PushLock,
    record: &RestoreRecord,
) -> Result<PathBuf, TargetError> {
    let paths = RecordPaths::new(home, project, CopyKind::Store);
    super::target::held(&paths, lock)?;
    let Some(completed_at) = record
        .completed_at
        .filter(|_| record.state == RestoreState::Completed)
    else {
        return Err(TargetError::Invalid {
            reason: "only a completed restore record is kept aside".into(),
        });
    };
    let current = restore_record_path(home, project);
    let stamp = completed_at.format("%Y%m%dT%H%M%SZ");
    let mut suffix = 0_u32;
    loop {
        let name = if suffix == 0 {
            format!("store.restore-{stamp}.json")
        } else {
            format!("store.restore-{stamp}-{suffix}.json")
        };
        let kept = paths.directory.join(name);
        match fs::symlink_metadata(&kept) {
            Ok(_) => suffix += 1,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // The push lock serializes every restore, so nothing takes
                // this name between the check and the move.
                fs::rename(&current, &kept).map_err(|source| TargetError::Io {
                    path: current.clone(),
                    source,
                })?;
                return Ok(kept);
            }
            Err(source) => return Err(TargetError::Io { path: kept, source }),
        }
    }
}

/// The name of the archive of a pending restore abandoned at `at`, or of the
/// `suffix`th one abandoned in the same second.
fn abandoned_name(at: DateTime<Utc>, suffix: u32) -> String {
    let stamp = at.format("%Y%m%dT%H%M%SZ");
    if suffix == 0 {
        format!("store.restore-abandoned-{stamp}.json")
    } else {
        format!("store.restore-abandoned-{stamp}-{suffix}.json")
    }
}

/// Archives the project's pending restore record, abandoned by `by` at `at`,
/// and then clears the active record. The archive is written whole to a
/// temporary file beside the record and then moved, without replacing
/// anything, to a name stamped with `at` in UTC,
/// `store.restore-abandoned-<YYYYMMDDTHHMMSSZ>.json`; a name already taken
/// moves on to the next free `-N` suffix. So a process that ends midway leaves
/// at most a temporary file, never a partial archive. Returns where it was
/// archived. When the active record cannot be cleared after that, a later
/// abandonment archives the record again under the next name.
///
/// # Errors
///
/// [`AbandonError::Archive`] for a lock taken for another kind or project, an
/// active record that is not a readable pending one, or an archive that could
/// not be written; the pending record then stays active.
/// [`AbandonError::Clear`] when the archive was written but the active record
/// could not be removed.
pub fn archive_abandoned_pending(
    home: &Path,
    project: &ProjectId,
    lock: &PushLock,
    by: &str,
    at: DateTime<Utc>,
) -> Result<Archived, AbandonError> {
    let paths = RecordPaths::new(home, project, CopyKind::Store);
    super::target::held(&paths, lock).map_err(AbandonError::Archive)?;
    let current = restore_record_path(home, project);
    let io_at = |path: &Path| {
        let path = path.to_path_buf();
        move |source| TargetError::Io { path, source }
    };
    let bytes = fs::read(&current)
        .map_err(io_at(&current))
        .map_err(AbandonError::Archive)?;
    let invalid = |reason: String| {
        AbandonError::Archive(TargetError::Invalid {
            reason: format!("{} cannot be abandoned: {reason}", current.display()),
        })
    };
    let record = parse(&bytes, project).map_err(invalid)?;
    if record.state != RestoreState::Pending {
        return Err(invalid("it is not a pending restore".into()));
    }
    let pending: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| invalid(error.to_string()))?;
    let envelope = AbandonedRestore {
        format_version: RECORD_FORMAT_VERSION,
        abandoned: Statement {
            by: by.to_owned(),
            at,
        },
        pending,
    };
    let mut body = serde_json::to_vec_pretty(&envelope)
        .map_err(|error| invalid(format!("its archive cannot be encoded: {error}")))?;
    body.push(b'\n');
    // A temporary file that cannot be removed after a failure is named with
    // that failure.
    let failed = |path: PathBuf, source: io::Error, left: Option<(PathBuf, io::Error)>| {
        let source = match left {
            None => source,
            Some((temporary, error)) => io::Error::new(
                source.kind(),
                format!(
                    "{source}; its temporary file {} could not be removed: {error}",
                    temporary.display()
                ),
            ),
        };
        AbandonError::Archive(TargetError::Io { path, source })
    };
    let mut temporary = tempfile::Builder::new()
        .prefix("store.restore-abandoned.")
        .suffix(".tmp")
        .tempfile_in(&paths.directory)
        .map_err(|source| failed(paths.directory.clone(), source, None))?;
    if let Err(source) =
        io::Write::write_all(&mut temporary, &body).and_then(|()| temporary.as_file().sync_all())
    {
        let path = temporary.path().to_path_buf();
        let left = temporary.close().err().map(|error| (path.clone(), error));
        return Err(failed(path, source, left));
    }
    let mut pending_archive = temporary.into_temp_path();
    let temporary = pending_archive.to_path_buf();
    let mut suffix = 0_u32;
    let archive = loop {
        let archive = paths.directory.join(abandoned_name(at, suffix));
        match pending_archive.persist_noclobber(&archive) {
            Ok(()) => break archive,
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                pending_archive = error.path;
                suffix += 1;
            }
            Err(error) => {
                let temporary = error.path.to_path_buf();
                let left = error.path.close().err().map(|left| (temporary, left));
                return Err(failed(archive, error.error, left));
            }
        }
    };
    // Outside Windows the move can be a hard link whose unlink failed, which
    // leaves the temporary file beside the archive; it is removed or named.
    #[cfg(test)]
    tests::after_archive_move(&archive, &temporary);
    let mut warnings = Vec::new();
    if fs::symlink_metadata(&temporary).is_ok() {
        #[cfg(test)]
        let removed = tests::remove_temporary(&temporary);
        #[cfg(not(test))]
        let removed = fs::remove_file(&temporary);
        if let Err(error) = removed {
            warnings.push(format!(
                "the archive's temporary file {} could not be removed: {error}",
                temporary.display()
            ));
        }
    }
    fs::remove_file(&current).map_err(|source| AbandonError::Clear {
        archive: archive.clone(),
        error: TargetError::Io {
            path: current.clone(),
            source,
        },
    })?;
    Ok(Archived { archive, warnings })
}

/// The commands that go on from a pending restore of `copy`: run it again
/// with the operator's statement, or abandon it, bound to that copy. A name is
/// given as `--option=value`, whose quoting both POSIX shells and PowerShell
/// read as one argument; a copy name holds only letters, digits and hyphens,
/// so it stays bare.
#[must_use]
pub fn pending_ways_on(copy: &str, retired_by: Option<&str>) -> String {
    let [retry, abandon] = pending_commands(copy, retired_by);
    format!("run `{retry}` again to finish it, or `{abandon}` to abandon it")
}

/// The two commands [`pending_ways_on`] names: the retry and the abandonment.
#[must_use]
pub fn pending_commands(copy: &str, retired_by: Option<&str>) -> [String; 2] {
    let copy = crate::shell::argument(copy);
    let by = retired_by.map_or_else(|| "NAME".to_owned(), crate::shell::argument);
    [
        format!("engram backup restore {copy} --origin-retired-by={by}"),
        format!("engram backup restore {copy} --abandon-pending --abandoned-by=NAME"),
    ]
}

#[cfg(test)]
mod tests;
