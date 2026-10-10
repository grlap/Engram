//! Directory candidates are explicit, immutable inputs, never Git or store backups.

use std::{collections::BTreeMap, fs, io::Read, path::Path};

use crate::task_sync::{TaskTree, invalid};
use anyhow::{Context, Result};

const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_TREE_BYTES: usize = 128 * 1024 * 1024;
const MAX_TASKS: usize = 100_000;

/// Read one complete directory without following links or ignoring unknown entries.
///
/// # Errors
/// Refuses partial, oversized, linked or malformed candidates.
pub fn read(path: &Path) -> Result<TaskTree> {
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
                let name = task
                    .file_name()
                    .into_string()
                    .map_err(|_| invalid("non-UTF8 task filename"))?;
                task_count += 1;
                if task_count > MAX_TASKS {
                    return Err(invalid("task candidate count limit exceeded").into());
                }
                add_document(
                    &mut documents,
                    &mut total_bytes,
                    format!("tasks/{name}"),
                    &task.path(),
                )?;
            }
        } else {
            return Err(invalid("unknown task directory entry").into());
        }
    }
    if !has_tasks {
        return Err(invalid("tasks directory missing").into());
    }
    Ok(TaskTree::from_documents(&documents)?)
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
        return Err(invalid("task candidate byte limit exceeded").into());
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
    let documents = tree.documents()?;
    if documents.len().saturating_sub(1) > MAX_TASKS
        || documents
            .values()
            .any(|bytes| bytes.len() as u64 > MAX_FILE_BYTES)
        || documents.values().map(Vec::len).sum::<usize>() > MAX_TREE_BYTES
    {
        return Err(invalid("export exceeds the supported task candidate limits").into());
    }
    if path.try_exists()? {
        if read(path)?.documents()? == documents {
            return Ok(());
        }
        return Err(invalid("export destination already contains a different candidate").into());
    }
    let parent = path
        .parent()
        .ok_or_else(|| invalid("export directory has no parent"))?;
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
            return Err(invalid("export destination appeared during preparation").into());
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
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(invalid("task candidate directory must be an ordinary directory").into());
    }
    Ok(())
}

fn read_file(path: &Path) -> Result<Vec<u8>> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return Err(invalid("task candidate file must be an ordinary bounded file").into());
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(invalid("task candidate file grew beyond byte limit").into());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests;
