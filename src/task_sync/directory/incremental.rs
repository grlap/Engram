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
}

impl FolderWrite {
    /// Retain this write before the head it publishes is recorded.
    ///
    /// # Errors
    /// Refuses a path outside the folder layout and I/O failures.
    pub fn retain(&self, data: &DataWorktree) -> Result<()> {
        for path in self.files.keys() {
            relative(path)?;
        }
        let bytes = serde_json::to_vec(self)?;
        let target = data.git_directory.join(JOURNAL);
        replace(&data.git_directory, &target, &bytes)
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
        for directory in [&folder, &folder.join("tasks")] {
            if !directory.try_exists()? {
                fs::create_dir(directory)?;
            }
            regular_directory(directory)?;
        }
        let (envelope, tasks): (Vec<_>, Vec<_>) = self
            .files
            .iter()
            .partition(|(path, _)| path.as_str() == "project.json");
        for (path, bytes) in tasks.into_iter().chain(envelope) {
            let target = folder.join(relative(path)?);
            match bytes {
                Some(bytes) => replace(&data.git_directory, &target, bytes.as_bytes())?,
                None => remove(&target)?,
            }
        }
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
            _ => Ok(()),
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

/// Write `bytes` beside nothing in the folder: stage the file in the
/// administration directory, sync it, and rename it over `target`.
fn replace(administration: &Path, target: &Path, bytes: &[u8]) -> Result<()> {
    if existing_file(target)? && fs::read(target)? == bytes {
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

fn remove(target: &Path) -> Result<()> {
    if existing_file(target)? {
        fs::remove_file(target)?;
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
