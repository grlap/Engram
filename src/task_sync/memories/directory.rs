//! Bounded memory files in the existing admitted task-data checkout.

use super::{FORMAT, Head, KeyHistory, ObjectId, Revision, validate_key};
use crate::task_sync::{data_worktree::DataWorktree, setup::PrivateDirectory};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io::Write, path::Path};

const FILE_LIMIT: usize = 1024 * 1024;
const TREE_LIMIT: usize = 128 * 1024 * 1024;
const ENTRY_LIMIT: usize = 100_000;
mod recovery;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HeadDocument {
    format: String,
    project_id: crate::ProjectId,
    key: String,
    head: Head,
}

/// Reads every selected key without following links. Import refuses unresolved
/// revision files even when Git merged their names without a text conflict.
///
/// # Errors
/// Refuses malformed, oversized, linked or structurally divergent selections.
pub fn read(data: &DataWorktree) -> Result<BTreeMap<String, KeyHistory>> {
    data.revalidate()?;
    let selected = data.select_folder(&data.checkout.join(".engram"))?;
    let root = PrivateDirectory::open(&selected)?;
    let result = read_root(&root)?;
    data.revalidate()?;
    Ok(result)
}

/// Reads a caller-retained pre-export folder as explicit recovery evidence.
/// No host-private mapping or publication checkpoint is created.
///
/// # Errors
/// Refuses unsafe, malformed or incomplete retained memory data.
pub fn read_basis(path: &Path) -> Result<BTreeMap<String, KeyHistory>> {
    read_root(&PrivateDirectory::open(path)?)
}

/// Reads only the unchanged task component while explicit memory recovery is
/// pending. Recovery separately validates every memory entry before changing it.
///
/// # Errors
/// Refuses unsafe files, unknown root entries and malformed task metadata.
pub fn read_tasks_for_recovery(
    data: &DataWorktree,
) -> Result<crate::task_sync::directory::FolderExchange> {
    Ok(crate::task_sync::directory::read_resolved_data_folder_for_memory_recovery(data)?.0)
}

/// Reads bounded caller-selected reconciliation evidence without following links.
///
/// # Errors
/// Refuses unsafe/oversized files or malformed per-key options.
pub fn read_options(path: &Path) -> Result<BTreeMap<String, super::ImportOptions>> {
    let absolute = std::path::absolute(path)?;
    let parent = PrivateDirectory::open(
        absolute
            .parent()
            .context("memory resolution parent missing")?,
    )?;
    let bytes = parent
        .read_bounded(
            Path::new(
                absolute
                    .file_name()
                    .context("memory resolution filename missing")?,
            ),
            FILE_LIMIT,
        )?
        .context("memory resolution file missing")?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub(in crate::task_sync) fn read_root(
    root: &PrivateDirectory,
) -> Result<BTreeMap<String, KeyHistory>> {
    Ok(read_root_with_bytes(root)?.0)
}

pub(in crate::task_sync) fn read_root_with_bytes(
    root: &PrivateDirectory,
) -> Result<(BTreeMap<String, KeyHistory>, usize)> {
    match root.dir().symlink_metadata("memories") {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((BTreeMap::new(), 0)),
        Err(e) => return Err(e.into()),
        Ok(_) => {}
    }
    let memories = root.child(Path::new("memories"))?;
    let envelope: serde_json::Value = serde_json::from_slice(
        &root
            .read_bounded(Path::new("project.json"), FILE_LIMIT)?
            .context("memory profile envelope missing")?,
    )?;
    ensure!(
        envelope
            .get("memory_profile")
            .and_then(serde_json::Value::as_str)
            == Some(FORMAT),
        "memory directory requires its versioned project capability"
    );
    let project = envelope
        .get("project")
        .and_then(|p| p.get("project_id"))
        .and_then(serde_json::Value::as_str)
        .context("memory profile project missing")?;
    let mut result = BTreeMap::new();
    let mut total = 0usize;
    for name in memories.names(ENTRY_LIMIT)? {
        let key = name.to_str().context("non-UTF8 memory key")?;
        validate_key(key)?;
        let directory = memories.child(Path::new(key))?;
        let raw = directory
            .read_bounded(Path::new("head.json"), FILE_LIMIT)?
            .context("memory head missing")?;
        total = total
            .checked_add(raw.len())
            .context("memory head byte overflow")?;
        ensure!(total <= TREE_LIMIT, "memory tree exceeds 128 MiB");
        let head: HeadDocument = serde_json::from_slice(&raw)?;
        ensure!(
            head.format == FORMAT && head.key == key,
            "memory head format/key mismatch"
        );
        let mut revisions = BTreeMap::new();
        for filename in directory.names(ENTRY_LIMIT)? {
            if filename == "head.json" {
                continue;
            }
            let filename = filename.to_str().context("non-UTF8 memory filename")?;
            let id: ObjectId = filename
                .strip_suffix(".json")
                .context("unknown memory directory entry")?
                .parse()
                .map_err(anyhow::Error::msg)?;
            ensure!(
                id.as_str().len() == 32,
                "memory filename must be a minted UUID"
            );
            let bytes = directory
                .read_bounded(Path::new(filename), FILE_LIMIT)?
                .context("memory revision missing")?;
            total = total
                .checked_add(bytes.len())
                .context("memory byte count overflow")?;
            ensure!(total <= TREE_LIMIT, "memory tree exceeds 128 MiB");
            let revision: Revision = serde_json::from_slice(&bytes)?;
            ensure!(
                revision.provenance.revision_id == id,
                "memory filename/id mismatch"
            );
            revisions.insert(id, revision);
        }
        let history = KeyHistory {
            format: head.format,
            project_id: head.project_id,
            key: head.key,
            head: head.head,
            revisions,
        };
        history.validate()?;
        ensure!(
            history.project_id.0 == project,
            "memory key belongs to another project"
        );
        result.insert(key.into(), history);
    }
    ensure!(total <= TREE_LIMIT, "memory tree exceeds 128 MiB");
    Ok((result, total))
}

pub(in crate::task_sync) fn decode_key(
    key: &str,
    documents: BTreeMap<String, Vec<u8>>,
) -> Result<KeyHistory> {
    validate_key(key)?;
    let head: HeadDocument =
        serde_json::from_slice(documents.get("head.json").context("memory head missing")?)?;
    ensure!(
        head.key == key && head.format == FORMAT,
        "memory head format/key mismatch"
    );
    let mut revisions = BTreeMap::new();
    for (name, bytes) in documents {
        if name == "head.json" {
            continue;
        }
        let id: ObjectId = name
            .strip_suffix(".json")
            .context("unknown memory file")?
            .parse()
            .map_err(anyhow::Error::msg)?;
        let revision: Revision = serde_json::from_slice(&bytes)?;
        ensure!(
            revision.provenance.revision_id == id,
            "memory filename/id mismatch"
        );
        revisions.insert(id, revision);
    }
    let history = KeyHistory {
        format: head.format,
        project_id: head.project_id,
        key: head.key,
        head: head.head,
        revisions,
    };
    history.validate()?;
    Ok(history)
}

/// Publishes deterministic revision bytes and updates heads last. Existing UUID
/// bytes are immutable. The caller's exact selected basis is rechecked under
/// the existing file-lock primitive before publication.
///
/// # Errors
/// Refuses stale inputs, changed immutable files and unsafe paths. Failed
/// publication retains its files for explicit recovery; it never deletes a key.
pub fn write(
    data: &DataWorktree,
    basis: &BTreeMap<String, KeyHistory>,
    selected: &BTreeMap<String, KeyHistory>,
) -> Result<()> {
    write_inner(data, basis, selected, false)
}

/// Recovers only files proved to be an interrupted publication of the explicit
/// retained basis to the newly recaptured canonical selection. Unknown bytes,
/// immutable rewrites and changed selected heads refuse without mutation.
///
/// # Errors
/// Refuses unrelated files or changes, unsafe paths and invalid recovery input.
pub fn recover(
    data: &DataWorktree,
    basis: &BTreeMap<String, KeyHistory>,
    selected: &BTreeMap<String, KeyHistory>,
) -> Result<()> {
    write_inner(data, basis, selected, true)
}

pub(in crate::task_sync) fn publication_lock(
    data: &DataWorktree,
) -> Result<crate::task_sync::file_lock::HeldLock> {
    let admin = PrivateDirectory::open(&data.git_directory)?;
    let lock = match admin.open_file(Path::new("memory-sync.lock"), true, false) {
        Ok(file) => file,
        Err(error) => {
            if !error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
            {
                return Err(error);
            }
            admin.open_file(Path::new("memory-sync.lock"), true, true)?
        }
    };
    Ok(crate::task_sync::file_lock::HeldLock::try_acquire(lock)?)
}

fn write_inner(
    data: &DataWorktree,
    basis: &BTreeMap<String, KeyHistory>,
    selected: &BTreeMap<String, KeyHistory>,
    recovering: bool,
) -> Result<()> {
    data.revalidate()?;
    let root = PrivateDirectory::open(&data.select_folder(&data.checkout.join(".engram"))?)?;
    let _held = publication_lock(data)?;
    for (key, old) in basis {
        old.validate()?;
        let new = selected
            .get(key)
            .context("memory export cannot omit a selected key")?;
        if matches!(old.head, Head::Tombstone { .. }) {
            ensure!(old == new, "permanent tombstone cannot be replaced");
        }
    }
    let mut total = 0usize;
    for (key, history) in selected {
        history.validate()?;
        ensure!(key == &history.key, "memory map key mismatch");
        let head = serde_json::to_vec_pretty(&HeadDocument {
            format: FORMAT.into(),
            project_id: history.project_id.clone(),
            key: key.clone(),
            head: history.head.clone(),
        })?;
        ensure!(head.len() <= FILE_LIMIT, "memory head exceeds 1 MiB");
        total = total
            .checked_add(head.len())
            .context("memory head byte overflow")?;
        for revision in history.revisions.values() {
            let bytes = serde_json::to_vec_pretty(revision)?;
            ensure!(bytes.len() <= FILE_LIMIT, "memory revision exceeds 1 MiB");
            total = total
                .checked_add(bytes.len())
                .context("memory tree size overflow")?;
        }
    }
    ensure!(total <= TREE_LIMIT, "memory tree exceeds 128 MiB");
    let (tasks, task_bytes) =
        crate::task_sync::directory::read_resolved_data_folder_for_memory_recovery(data)?;
    ensure!(
        selected
            .values()
            .chain(basis.values())
            .all(|history| history.project_id == tasks.tree.project.project_id),
        "memory publication belongs to another project"
    );
    ensure!(
        total
            .checked_add(task_bytes)
            .is_some_and(|bytes| bytes <= TREE_LIMIT),
        "combined task and memory tree exceeds 128 MiB"
    );
    if recovering {
        recovery::prepare(&root, basis, selected)?;
    } else {
        ensure!(
            read_root(&root)? == *basis,
            "selected memory export basis changed; an interrupted publication requires --memory-recovery-basis"
        );
    }
    let memories = match root.child(Path::new("memories")) {
        Ok(child) => child,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            root.create_child(Path::new("memories"))?
        }
        Err(error) => return Err(error),
    };
    for (key, history) in selected {
        let directory = match memories.child(Path::new(key)) {
            Ok(child) => child,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                memories.create_child(Path::new(key))?
            }
            Err(error) => return Err(error),
        };
        for (id, revision) in &history.revisions {
            let name = format!("{id}.json");
            let bytes = serde_json::to_vec_pretty(revision)?;
            ensure!(bytes.len() <= FILE_LIMIT, "memory revision exceeds 1 MiB");
            if let Some(existing) = directory.read_bounded(Path::new(&name), FILE_LIMIT)? {
                ensure!(existing == bytes, "immutable shared revision bytes changed");
            } else {
                let pending = format!("revision-{id}.pending");
                let mut file = directory.open_file(Path::new(&pending), true, true)?;
                file.write_all(&bytes)?;
                file.sync_all()?;
                drop(file);
                directory
                    .dir()
                    .hard_link(&pending, directory.dir(), &name)?;
                directory.dir().remove_file(&pending)?;
            }
        }
        let head = serde_json::to_vec_pretty(&HeadDocument {
            format: FORMAT.into(),
            project_id: history.project_id.clone(),
            key: key.clone(),
            head: history.head.clone(),
        })?;
        let temporary = format!("head-{}.pending", uuid::Uuid::new_v4());
        let mut file = directory.open_file(Path::new(&temporary), true, true)?;
        file.write_all(&head)?;
        file.sync_all()?;
        drop(file);
        directory
            .dir()
            .rename(&temporary, directory.dir(), "head.json")?;
        // Only old, validated selected revision files are removed. Their Git
        // history and original canonical local histories remain unchanged.
        if let Some(old) = basis.get(key) {
            for id in old
                .revisions
                .keys()
                .filter(|id| !history.revisions.contains_key(*id))
            {
                match directory.dir().remove_file(format!("{id}.json")) {
                    Ok(()) => {}
                    Err(error) if recovering && error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
    }
    ensure!(
        read_root(&root)? == *selected,
        "memory publication did not match selected content"
    );
    data.revalidate()?;
    Ok(())
}
