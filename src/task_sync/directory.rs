//! Directory candidates are explicit, immutable inputs, never Git or store backups.

use std::{collections::BTreeMap, fs, io::Read, path::Path};

use crate::task_sync::{TaskSyncRefusal, TaskTree};
use anyhow::{Context, Result};

const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_TREE_BYTES: usize = 128 * 1024 * 1024;
const MAX_TASKS: usize = 100_000;

/// Read one complete directory without following links or ignoring unknown entries.
///
/// # Errors
/// Refuses partial, oversized, linked or malformed candidates.
pub fn read(path: &Path) -> Result<TaskTree> {
    let tree = read_stored(path)?;
    tree.validate()
        .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
    Ok(tree)
}

fn read_stored(path: &Path) -> Result<TaskTree> {
    Ok(stored_tree(&read_documents(path)?)?)
}

fn stored_tree(
    documents: &BTreeMap<String, Vec<u8>>,
) -> Result<TaskTree, crate::storage::StoreError> {
    TaskTree::from_stored_documents(documents)
        .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))
}

/// Read retained bytes without applying today's note disclosure admission.
pub(crate) fn read_documents(path: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    regular_directory(path)?;
    let mut documents = BTreeMap::new();
    let mut total_bytes = 0;
    let mut task_count = 0;
    let mut has_tasks = false;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "project.json" {
            add_document(
                &mut documents,
                &mut total_bytes,
                "project.json".into(),
                &entry.path(),
            )?;
        } else if name == "tasks" {
            has_tasks = true;
            regular_directory(&entry.path())?;
            for task in fs::read_dir(entry.path())? {
                let task = task?;
                let name = task.file_name().into_string().map_err(|_| {
                    TaskSyncRefusal::DestinationNonUtf8Name.invalid("non-UTF8 task filename")
                })?;
                task_count += 1;
                if task_count > MAX_TASKS {
                    return Err(TaskSyncRefusal::DestinationTaskLimit
                        .invalid("task candidate count limit exceeded")
                        .into());
                }
                add_document(
                    &mut documents,
                    &mut total_bytes,
                    format!("tasks/{name}"),
                    &task.path(),
                )?;
            }
        } else {
            return Err(TaskSyncRefusal::DestinationUnknownEntry
                .invalid("unknown task directory entry")
                .into());
        }
    }
    if !has_tasks {
        return Err(TaskSyncRefusal::DestinationMissingTasks
            .invalid("tasks directory missing")
            .into());
    }
    stored_tree(&documents)?;
    Ok(documents)
}

/// Preflight existing output without recording a pending operation.
pub(crate) fn matching_destination(
    path: &Path,
    documents: &BTreeMap<String, Vec<u8>>,
) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) if read_documents(path)? == *documents => Ok(true),
        Ok(_) => Err(occupied().into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Refuse documents that `write_documents` could never publish. It reads
/// nothing from disk, so a caller can check it before changing anything.
pub(crate) fn admit_documents(documents: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    let tree = stored_tree(documents)?;
    if tree.project.notes_included && tree.tasks.is_empty() {
        return Err(TaskSyncRefusal::InvalidGraph
            .invalid("notes_included requires at least one task")
            .into());
    }
    admit_limits(documents)
}

/// Refuse documents over a candidate limit, naming the first limit exceeded.
fn admit_limits(documents: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    let refusal = if documents.len().saturating_sub(1) > MAX_TASKS {
        TaskSyncRefusal::DestinationTaskLimit
    } else if documents
        .values()
        .any(|bytes| bytes.len() as u64 > MAX_FILE_BYTES)
    {
        TaskSyncRefusal::DestinationFileLimit
    } else if documents.values().map(Vec::len).sum::<usize>() > MAX_TREE_BYTES {
        TaskSyncRefusal::DestinationTreeLimit
    } else {
        return Ok(());
    };
    Err(refusal
        .invalid("export exceeds the supported task candidate limits")
        .into())
}

fn occupied() -> crate::storage::StoreError {
    TaskSyncRefusal::DestinationOccupied
        .invalid("export destination already contains a different candidate")
}

fn no_parent() -> crate::storage::StoreError {
    TaskSyncRefusal::DestinationNoParent.invalid("export directory has no parent")
}

/// Publish already selected documents, preserving exact bytes on retry.
pub(crate) fn write_documents(path: &Path, documents: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    admit_documents(documents)?;
    let absolute = std::path::absolute(path)?;
    let path = absolute.as_path();
    if matching_destination(path, documents)? {
        return Ok(());
    }
    let parent = path.parent().ok_or_else(no_parent)?;
    regular_directory(parent)?;
    let staging = parent.join(format!(".engram-task-candidate-{}", uuid::Uuid::now_v7()));
    fs::create_dir(&staging)?;
    let result = (|| {
        fs::create_dir(staging.join("tasks"))?;
        for (relative, bytes) in documents {
            use std::io::Write;
            let mut file = fs::File::create(staging.join(relative))?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        let mut pending = tempfile::TempPath::try_from_path(&staging)?;
        pending.disable_cleanup(true);
        pending
            .persist_noclobber(path)
            .map_err(|error| error.error)?;
        Ok(())
    })();
    if result.is_err() {
        // The UUID staging path was created by this call under the checked parent.
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn add_document(
    documents: &mut BTreeMap<String, Vec<u8>>,
    total: &mut usize,
    name: String,
    path: &Path,
) -> Result<()> {
    let bytes = read_file(path).with_context(|| format!("reading {}", path.display()))?;
    *total += bytes.len();
    if *total > MAX_TREE_BYTES {
        return Err(TaskSyncRefusal::DestinationTreeLimit
            .invalid("task candidate byte limit exceeded")
            .into());
    }
    documents.insert(name, bytes);
    Ok(())
}

/// Publish a validated directory once. Existing divergent candidates are preserved.
///
/// # Errors
/// Refuses overwrite, links, invalid data, or an I/O failure.
pub fn write(path: &Path, tree: &TaskTree) -> Result<()> {
    let absolute = std::path::absolute(path)?;
    let path = absolute.as_path();
    let documents = tree
        .documents()
        .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
    admit_limits(&documents)?;
    if path.try_exists()? {
        let existing = read_stored(path)?
            .documents()
            .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
        if existing == documents {
            return Ok(());
        }
        return Err(occupied().into());
    }
    let parent = path.parent().ok_or_else(no_parent)?;
    regular_directory(parent)?;
    let staging = parent.join(format!(".engram-task-candidate-{}", uuid::Uuid::now_v7()));
    fs::create_dir(&staging)?;
    let result = (|| {
        fs::create_dir(staging.join("tasks"))?;
        for (relative, bytes) in documents {
            fs::write(staging.join(relative), bytes)?;
        }
        // Never replace an existing directory, even one created after the first check.
        if path.try_exists()? {
            return Err(TaskSyncRefusal::DestinationAppeared
                .invalid("export destination appeared during preparation")
                .into());
        }
        // Use the existing no-clobber publication primitive. Systems that
        // cannot rename a directory without replacement fail closed.
        let mut pending = tempfile::TempPath::try_from_path(&staging)?;
        pending.disable_cleanup(true);
        pending
            .persist_noclobber(path)
            .map_err(|error| error.error)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn regular_directory(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    let refusal = if meta.file_type().is_symlink() {
        TaskSyncRefusal::DestinationLink
    } else if !meta.is_dir() {
        TaskSyncRefusal::DestinationNotDirectory
    } else {
        return Ok(());
    };
    Err(refusal
        .invalid("task candidate directory must be an ordinary directory")
        .into())
}

fn read_file(path: &Path) -> Result<Vec<u8>> {
    let meta = fs::symlink_metadata(path)?;
    let refusal = if meta.file_type().is_symlink() {
        Some(TaskSyncRefusal::DestinationLink)
    } else if !meta.is_file() {
        Some(TaskSyncRefusal::DestinationNotFile)
    } else if meta.len() > MAX_FILE_BYTES {
        Some(TaskSyncRefusal::DestinationFileLimit)
    } else {
        None
    };
    if let Some(refusal) = refusal {
        return Err(refusal
            .invalid("task candidate file must be an ordinary bounded file")
            .into());
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(TaskSyncRefusal::DestinationFileGrew
            .invalid("task candidate file grew beyond byte limit")
            .into());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests;
