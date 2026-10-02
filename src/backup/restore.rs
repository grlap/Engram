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

#[cfg(test)]
mod tests;
