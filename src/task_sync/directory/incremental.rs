//! Write an incremental export into the existing data folder in place.
//!
//! Only the named paths are touched: each written file goes through a
//! temporary file in the worktree's Git administration directory and is
//! renamed over its path, each removal deletes one file, and the envelope is
//! written last. A journal of the exact operations is retained in the same
//! administration directory before the folder head is recorded, so an
//! interrupted publication is finished by repeating the same operations.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Seek, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::TaskSyncRefusal;
use crate::{CanonicalObject, ObjectId, task_sync::data_worktree::DataWorktree};

const JOURNAL: &str = "engram-task-sync-folder-write.json";
const STAGING: &str = "engram-task-sync-folder-staging";
const KEYED_JOURNAL_PREFIX: &[u8] = b"{\"header\":";

#[cfg(test)]
thread_local! {
    pub(crate) static BEFORE_EXPORT_COMPLETION: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
    static FOLDER_WRITE_PAYLOAD_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn take_folder_write_payload_reads() -> usize {
    FOLDER_WRITE_PAYLOAD_READS.with(std::cell::Cell::take)
}

/// The Git parent observed before this export wrote to the data worktree.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportParent {
    Unborn,
    Commit(String),
}

/// The exact file operations of one export, relative to the `.engram`
/// folder: `tasks/<id>.json` or `project.json`, `None` removing a file.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FolderWrite {
    /// The local store/project journal slot used for this write.
    #[serde(skip)]
    pub owner: Option<ObjectId>,
    /// What the caller recorded before publication, compared on recovery.
    pub head: serde_json::Value,
    /// The minted record key of that head, if this journal recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_id: Option<ObjectId>,
    pub files: BTreeMap<String, Option<String>>,
    /// Each written path's content fingerprint before the write, `None` for
    /// an absent file. Recovery replays only over these preimages, or over
    /// files already published, so it never overwrites a later edit.
    pub before: BTreeMap<String, Option<String>>,
    /// The recorded Git parent is required to replay or confirm an owned write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ExportParent>,
    /// Paths whose postimages differ from the commit named by `parent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_paths: Option<BTreeSet<String>>,
    /// A finished publication is retained until its commit is confirmed.
    #[serde(default)]
    pub published: bool,
    /// Postimages already admitted by the separate memory publisher.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub memory_files: BTreeMap<String, Option<String>>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FolderWriteHeader {
    id: ObjectId,
    fingerprint: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct KeyedFolderWrite<T> {
    header: FolderWriteHeader,
    write: T,
}

pub(crate) struct FolderWriteReader {
    file: File,
    owner: Option<ObjectId>,
}

impl FolderWriteReader {
    pub(crate) fn head_key(&mut self) -> Result<Option<(ObjectId, String)>> {
        // Inspect only the bounded header. An older journal without one
        // falls back to the full reader and its original validation.
        let mut prefix = Vec::with_capacity(256);
        (&mut self.file).take(256).read_to_end(&mut prefix)?;
        let Some(prefix) = prefix.strip_prefix(KEYED_JOURNAL_PREFIX) else {
            return Ok(None);
        };
        let mut fields =
            serde_json::Deserializer::from_slice(prefix).into_iter::<FolderWriteHeader>();
        Ok(fields.next().and_then(Result::ok).and_then(|header| {
            prefix[fields.byte_offset()..]
                .starts_with(b",\"write\":{")
                .then_some((header.id, header.fingerprint))
        }))
    }

    pub(crate) fn read(mut self) -> Result<FolderWrite> {
        self.file.rewind()?;
        #[cfg(test)]
        FOLDER_WRITE_PAYLOAD_READS.with(|count| count.set(count.get() + 1));
        let mut bytes = Vec::new();
        self.file.read_to_end(&mut bytes)?;
        let mut write: FolderWrite = if bytes.starts_with(KEYED_JOURNAL_PREFIX) {
            serde_json::from_slice::<KeyedFolderWrite<FolderWrite>>(&bytes)?.write
        } else {
            serde_json::from_slice(&bytes)?
        };
        write.owner = self.owner;
        Ok(write)
    }
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
        let bytes = if let Some(id) = &self.head_id {
            let fingerprint = CanonicalObject::freeze(&self.head)?
                .key()
                .as_str()
                .to_owned();
            serde_json::to_vec(&KeyedFolderWrite {
                header: FolderWriteHeader {
                    id: id.clone(),
                    fingerprint,
                },
                write: self,
            })?
        } else {
            serde_json::to_vec(self)?
        };
        let target = journal_path(data, self.owner.as_ref());
        replace(&data.git_directory, &target, &bytes)?;
        // The journal is durable before the head it replays is recorded.
        sync_parent(&target)
    }

    /// The retained write, if one was left by an interrupted export.
    ///
    /// # Errors
    /// Refuses an unreadable or malformed journal.
    pub fn pending(data: &DataWorktree, owner: Option<&ObjectId>) -> Result<Option<Self>> {
        Self::open(data, owner)?
            .map(FolderWriteReader::read)
            .transpose()
    }

    pub(crate) fn open(
        data: &DataWorktree,
        owner: Option<&ObjectId>,
    ) -> Result<Option<FolderWriteReader>> {
        match File::open(journal_path(data, owner)) {
            Ok(file) => Ok(Some(FolderWriteReader {
                file,
                owner: owner.cloned(),
            })),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn retained_bytes(&self, data: &DataWorktree) -> Result<Option<Vec<u8>>> {
        match fs::read(journal_path(data, self.owner.as_ref())) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn restore_retained(
        &self,
        data: &DataWorktree,
        previous: Option<&[u8]>,
    ) -> Result<()> {
        if let Some(bytes) = previous {
            let target = journal_path(data, self.owner.as_ref());
            replace(&data.git_directory, &target, bytes)?;
            sync_parent(&target)
        } else {
            Self::discard(data, self.owner.as_ref())
        }
    }

    /// An older unowned slot cannot establish who may replay or remove it.
    pub(crate) fn require_no_unowned_journal(data: &DataWorktree) -> Result<()> {
        let path = journal_path(data, None);
        match fs::symlink_metadata(&path) {
            Ok(_) => Err(TaskSyncRefusal::IncrementalBasisUnavailable
                .invalid(format!(
                    "unowned export journal at {}; finish that interrupted export with its originating store and build",
                    path.display()
                ))
                .into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Apply every operation idempotently. Retain a completed export for
    /// confirmation when it has an exact expected Git parent.
    ///
    /// # Errors
    /// Refuses a changed worktree, a linked or irregular path and I/O failures.
    pub fn publish(self, data: &DataWorktree) -> Result<()> {
        self.publish_with_memories(data, || Ok(()))
    }

    pub(crate) fn publish_with_memories(
        mut self,
        data: &DataWorktree,
        publish_memories: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        self.publish_files(data)?;
        publish_memories()?;
        #[cfg(test)]
        if let Some(hook) = BEFORE_EXPORT_COMPLETION.with(|hook| hook.borrow_mut().take()) {
            hook();
        }
        if !data.publication_tip_matches()? {
            return Err(self.moved_interrupted_export()?);
        }
        self.published = true;
        self.retain(data)
    }

    pub(crate) fn writes_memories(&self) -> Result<bool> {
        if !self.memory_files.is_empty() {
            return Ok(true);
        }
        self.files
            .get("project.json")
            .and_then(Option::as_deref)
            .map(|bytes| super::committed_memory_envelope(bytes.as_bytes(), "retained export"))
            .transpose()
            .map(|envelope| envelope.is_some_and(|(_, capable)| capable))
    }

    pub(crate) fn restore_combined_export(&self) -> anyhow::Error {
        if self.parent == Some(ExportParent::Unborn) {
            return import_interrupted_root(true);
        }
        moved(&format!(
            "preserve the current tip and exported files, {}, then export this data directory on the same branch with this host's original notes settings and alias. Use `--rebuild`, `--include-memories` and `--memory-recovery-basis OWN_EXPORT`",
            self.restore_original_basis()
        ))
    }

    fn restore_original_basis(&self) -> String {
        match &self.parent {
            Some(ExportParent::Commit(commit)) => {
                format!("restore the original basis at data commit {commit}")
            }
            _ => "restore the original basis".to_owned(),
        }
    }

    pub(crate) fn moved_interrupted_export(&self) -> Result<anyhow::Error> {
        Ok(if self.writes_memories()? {
            self.restore_combined_export()
        } else if self.parent == Some(ExportParent::Unborn) {
            import_interrupted_root(false)
        } else {
            moved(&format!(
                "preserve the current tip and exported files, {}, then run `engram task-sync export` to finish the interrupted write",
                self.restore_original_basis()
            ))
        })
    }

    /// Refuse a conflict already present anywhere in the interrupted write
    /// before replay changes any file.
    pub(crate) fn replay(self, data: &DataWorktree) -> Result<()> {
        self.validate_replay(data)?;
        self.publish(data)
    }

    pub(crate) fn validate_replay(&self, data: &DataWorktree) -> Result<()> {
        // A fresh export checked this before it was recorded; a replay checks
        // again, so it never commits less than it writes either.
        data.refuse_ignored_files(
            self.files
                .iter()
                .filter(|(_, bytes)| bytes.is_some())
                .map(|(path, _)| path.as_str()),
        )?;
        let folder = data.checkout.join(".engram");
        for directory in [&folder, &folder.join("tasks")] {
            match fs::symlink_metadata(directory) {
                Ok(_) => regular_directory(directory)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        for (path, bytes) in &self.files {
            self.check_preimage(&folder.join(relative(path)?), path, bytes.as_deref())?;
        }
        Ok(())
    }

    fn publish_files(&self, data: &DataWorktree) -> Result<()> {
        data.check_head()?;
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
            self.check_preimage(&target, path, bytes.as_deref())?;
            match bytes {
                Some(bytes) => replace(&data.git_directory, &target, bytes.as_bytes())?,
                None => remove(&target)?,
            }
        }
        // The journal goes only once every rename and removal it names is
        // durable, including one an interrupted earlier attempt already made
        // and this attempt therefore found in place.
        sync_directory(&folder.join("tasks"))?;
        sync_directory(&folder)
    }

    fn check_preimage(&self, target: &Path, path: &str, bytes: Option<&str>) -> Result<()> {
        let current = fingerprint_target(target)?;
        let published = bytes.map(|bytes| fingerprint(bytes.as_bytes()));
        if current != published && self.before.get(path) != Some(&current) {
            return Err(TaskSyncRefusal::IncrementalBasisUnavailable
                .invalid(format!(
                    "the data folder's {path} changed after this export was planned; nothing more was written. Preserve that change, restore the file to what it held before, and export again"
                ))
                .into());
        }
        Ok(())
    }

    /// Drop the journal of a write whose head was never recorded; the folder
    /// was not touched before the head.
    ///
    /// # Errors
    /// Refuses I/O failures other than an absent journal.
    pub fn discard(data: &DataWorktree, owner: Option<&ObjectId>) -> Result<()> {
        match fs::remove_file(journal_path(data, owner)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
            // The removal is made durable, so a crash cannot bring back a
            // journal of a write already finished or dropped.
            Ok(()) => sync_directory(&data.git_directory),
            Err(_) => Ok(()),
        }
    }
}

fn moved(reason: &str) -> anyhow::Error {
    TaskSyncRefusal::IncrementalBasisUnavailable
        .invalid(reason.to_owned())
        .into()
}

fn import_interrupted_root(combined: bool) -> anyhow::Error {
    let memories = if combined { " --include-memories" } else { "" };
    moved(&format!(
        "the interrupted export began on an unborn data branch; preserve the current tip and set aside any loose files, then import the clean committed folder with `engram task-sync import --basis OWN_EXPORT/project.json{memories}` using your saved pre-merge envelope"
    ))
}

fn journal_path(data: &DataWorktree, owner: Option<&ObjectId>) -> PathBuf {
    let name = owner.map_or_else(
        || JOURNAL.to_owned(),
        |owner| format!("engram-task-sync-folder-write-{}.json", owner.as_str()),
    );
    data.git_directory.join(name)
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
