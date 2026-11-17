//! Write an incremental export into the existing data folder in place.
//!
//! Only the named paths are touched: each written file goes through a
//! temporary file in the worktree's Git administration directory and is
//! renamed over its path, each removal deletes one file, and the envelope is
//! written last. A journal of the exact operations is retained in the same
//! administration directory before the folder head is recorded, so an
//! interrupted publication is finished by repeating the same operations.

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::TaskSyncRefusal;
use crate::task_sync::data_worktree::DataWorktree;

const JOURNAL: &str = "engram-task-sync-folder-write.json";
const STAGING: &str = "engram-task-sync-folder-staging";

/// The exact file operations of one export, relative to the `.engram`
/// folder: `tasks/<id>.json` or `project.json`, `None` removing a file.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FolderWrite {
    /// What the caller recorded before publication, compared on recovery.
    pub head: serde_json::Value,
    pub files: BTreeMap<String, Option<String>>,
    /// Each written path's content fingerprint before the write, `None` for
    /// an absent file. Recovery replays only over these preimages, or over
    /// files already published, so it never overwrites a later edit.
    pub before: BTreeMap<String, Option<String>>,
}

impl FolderWrite {
    /// Retain this write before the head it publishes is recorded.
    ///
    /// # Errors
    /// Refuses a path outside the folder layout, a preimage set that does not
    /// name exactly the written paths, and I/O failures.
    pub fn retain(&self, data: &DataWorktree) -> Result<()> {
        for path in self.files.keys() {
            relative(path)?;
        }
        anyhow::ensure!(
            self.before.keys().eq(self.files.keys()),
            "folder write preimages must name exactly its written paths"
        );
        sweep(&data.git_directory.join(STAGING))?;
        let bytes = serde_json::to_vec(self)?;
        let target = data.git_directory.join(JOURNAL);
        replace(&data.git_directory, &target, &bytes)?;
        // The journal is durable before the head it replays is recorded.
        sync_parent(&target)
    }

    /// The retained write, if one was left by an interrupted export.
    ///
    /// # Errors
    /// Refuses an unreadable or malformed journal.
    pub fn pending(data: &DataWorktree) -> Result<Option<Self>> {
        match fs::read(data.git_directory.join(JOURNAL)) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Apply every operation, idempotently, then drop the journal.
    ///
    /// # Errors
    /// Refuses a changed worktree, a linked or irregular path and I/O failures.
    pub fn publish(&self, data: &DataWorktree) -> Result<()> {
        data.revalidate()?;
        let folder = data.checkout.join(".engram");
        // A bootstrap may find no folder yet; an existing one must be ordinary.
        // Each directory's own entry is made durable in its parent before the
        // journal that could replay this write is dropped, including one an
        // interrupted attempt created without syncing it.
        for directory in [&folder, &folder.join("tasks")] {
            if !directory.try_exists()? {
                fs::create_dir(directory)?;
            }
            regular_directory(directory)?;
            sync_parent(directory)?;
        }
        // Removals go first, then writes that shrink a file before those that
        // grow one, so the task files never hold more tasks or bytes on the way
        // than at either end. The envelope goes last, so a shrinking envelope
        // can leave the folder at most its own size difference over that until
        // it is written; an interruption there is finished from the journal.
        let (envelope, tasks): (Vec<_>, Vec<_>) = self
            .files
            .iter()
            .partition(|(path, _)| path.as_str() == "project.json");
        let (removals, writes): (Vec<_>, Vec<_>) =
            tasks.into_iter().partition(|(_, bytes)| bytes.is_none());
        let mut writes = writes
            .into_iter()
            .map(|(path, bytes)| {
                let target = folder.join(relative(path)?);
                let old = match fs::symlink_metadata(&target) {
                    Ok(meta) => i128::from(meta.len()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
                    Err(error) => return Err(error.into()),
                };
                let new = i128::try_from(bytes.as_deref().map_or(0, str::len))?;
                Ok((new - old, (path, bytes)))
            })
            .collect::<Result<Vec<_>>>()?;
        writes.sort_by_key(|(growth, _)| *growth);
        let writes = writes.into_iter().map(|(_, write)| write);
        for (path, bytes) in removals.into_iter().chain(writes).chain(envelope) {
            let target = folder.join(relative(path)?);
            // Each path must still hold its preimage, or already the bytes
            // this write publishes, right before it is replaced or removed, so
            // an edit made since the export was planned is not overwritten. An
            // outside write landing between this check and the rename itself
            // is not fenced: no tool outside Engram honours its folder lock.
            let current = fingerprint_target(&target)?;
            let published = bytes.as_deref().map(|bytes| fingerprint(bytes.as_bytes()));
            if current != published && self.before.get(path) != Some(&current) {
                return Err(TaskSyncRefusal::IncrementalBasisUnavailable
                    .invalid(format!(
                        "the data folder's {path} changed after this export was planned; nothing more was written. Preserve that change, restore the file to what it held before, and export again"
                    ))
                    .into());
            }
            match bytes {
                Some(bytes) => replace(&data.git_directory, &target, bytes.as_bytes())?,
                None => remove(&target)?,
            }
        }
        // The journal goes only once every rename and removal it names is
        // durable, including one an interrupted earlier attempt already made
        // and this attempt therefore found in place.
        sync_directory(&folder.join("tasks"))?;
        sync_directory(&folder)?;
        data.revalidate()?;
        Self::discard(data)
    }

    /// Drop the journal of a write whose head was never recorded; the folder
    /// was not touched before the head.
    ///
    /// # Errors
    /// Refuses I/O failures other than an absent journal.
    pub fn discard(data: &DataWorktree) -> Result<()> {
        match fs::remove_file(data.git_directory.join(JOURNAL)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
            // The removal is made durable, so a crash cannot bring back a
            // journal of a write already finished or dropped.
            Ok(()) => sync_directory(&data.git_directory),
            Err(_) => Ok(()),
        }
    }
}

/// Only the folder's own layout: the envelope or one canonical task file.
fn relative(path: &str) -> Result<PathBuf> {
    if path == "project.json" {
        return Ok(PathBuf::from(path));
    }
    let id = path
        .strip_prefix("tasks/")
        .and_then(|name| name.strip_suffix(".json"))
        .and_then(|id| uuid::Uuid::parse_str(id).ok());
    match id {
        Some(id) if path == format!("tasks/{id}.json") => {
            Ok(Path::new("tasks").join(format!("{id}.json")))
        }
        _ => Err(TaskSyncRefusal::DestinationUnknownEntry
            .invalid("folder write names a path outside the task folder layout")
            .into()),
    }
}

fn regular_directory(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.file_type().is_symlink() || reparse(&meta) {
        return Err(TaskSyncRefusal::DestinationNotDirectory
            .invalid("task folder entry must be an ordinary directory")
            .into());
    }
    Ok(())
}

fn existing_file(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() && !reparse(&meta) => Ok(true),
        Ok(_) => Err(TaskSyncRefusal::DestinationNotFile
            .invalid("task folder entry must be an ordinary file")
            .into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// A file's content fingerprint, the same comparison the export records as
/// each path's preimage: `None` for an absent file.
fn fingerprint(bytes: &[u8]) -> String {
    crate::ObjectId::from_canonical_bytes(bytes).to_string()
}

fn fingerprint_target(target: &Path) -> Result<Option<String>> {
    if !existing_file(target)? {
        return Ok(None);
    }
    if fs::metadata(target)?.len() > super::MAX_FILE_BYTES {
        return Err(TaskSyncRefusal::DestinationFileLimit
            .invalid("task folder file is larger than the per-file limit")
            .into());
    }
    Ok(Some(fingerprint(&fs::read(target)?)))
}

/// Write `bytes` beside nothing in the folder: stage the file in the
/// administration directory, sync it, and rename it over `target`. The
/// caller syncs the folder's directories before it drops the journal.
fn replace(administration: &Path, target: &Path, bytes: &[u8]) -> Result<()> {
    // Only a file of the same length is read whole to compare it.
    if existing_file(target)?
        && fs::metadata(target)?.len() == bytes.len() as u64
        && fs::read(target)? == bytes
    {
        return Ok(());
    }
    let staging = administration.join(STAGING);
    fs::create_dir_all(&staging)?;
    regular_directory(&staging)?;
    let temporary = staging.join(uuid::Uuid::now_v7().to_string());
    let result = (|| {
        let mut file = fs::File::create_new(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, target).context("publish one task folder file")
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// The caller syncs the folder's directories before it drops the journal.
fn remove(target: &Path) -> Result<()> {
    if existing_file(target)? {
        fs::remove_file(target)?;
    }
    Ok(())
}

/// Make a rename or removal durable: on Unix a directory entry survives a
/// power loss only once its directory is synced.
fn sync_parent(target: &Path) -> Result<()> {
    match target.parent() {
        Some(parent) => sync_directory(parent),
        None => Ok(()),
    }
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> Result<()> {
    fs::File::open(directory)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
#[allow(
    clippy::unnecessary_wraps,
    reason = "the Unix form can fail, and the callers propagate either way"
)]
fn sync_directory(_: &Path) -> Result<()> {
    Ok(())
}

/// Remove the temporary files an interrupted write left in the staging
/// directory. The folder lock is held, so no other write is staging there.
fn sweep(staging: &Path) -> Result<()> {
    if !staging.try_exists()? {
        return Ok(());
    }
    regular_directory(staging)?;
    for entry in fs::read_dir(staging)? {
        let entry = entry?;
        let staged = entry
            .file_name()
            .to_str()
            .is_some_and(|name| uuid::Uuid::parse_str(name).is_ok());
        if staged && existing_file(&entry.path())? {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn reparse(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn reparse(_: &fs::Metadata) -> bool {
    false
}
