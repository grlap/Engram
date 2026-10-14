//! Whole-operation disclosure admission and bounded raw document retention.

use super::super::files::Directory;
use crate::task_sync::{TaskTree, notes};
use anyhow::{Context, Result, ensure};
use std::{collections::BTreeMap, io::Write, path::Path};

pub(super) type Documents = BTreeMap<String, Vec<u8>>;

pub(super) fn disclose(documents: &Documents) -> Result<TaskTree> {
    let tree = TaskTree::from_stored_documents(documents)?;
    let refused: Vec<_> = tree
        .tasks
        .iter()
        .filter(|task| {
            notes::restricted(&task.labels)
                || notes::validate_notes(&task.notes, &task.labels).is_err()
                || crate::task_sync::activity::validate_disclosure(&task.activity, &task.labels)
                    .is_err()
        })
        .collect();
    if !refused.is_empty() {
        let ids = refused
            .iter()
            .take(16)
            .map(|task| task.id.0.to_string())
            .collect::<Vec<_>>()
            .join(",");
        anyhow::bail!(
            "task disclosure refused: count={}, task_ids=[{}], omitted_ids={}; protected labels or retained note/activity admission; no field values disclosed",
            refused.len(),
            ids,
            refused.len().saturating_sub(16)
        );
    }
    ensure!(
        documents
            .values()
            .all(|bytes| bytes.ends_with(b"\n") && !bytes.contains(&b'\r')),
        "materialization requires retained LF document bytes; it never rewrites the candidate"
    );
    Ok(tree)
}

pub(super) fn write(directory: &Directory, documents: &Documents) -> Result<()> {
    let tasks = directory.create_child(Path::new("tasks"))?;
    for (path, bytes) in documents {
        let (parent, name) = if path == "project.json" {
            (directory, path.as_str())
        } else {
            (
                &tasks,
                path.strip_prefix("tasks/")
                    .context("unexpected candidate document")?,
            )
        };
        let mut file = parent.open_file(Path::new(name), true, true)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    Ok(())
}

pub(super) fn read(directory: &Directory) -> Result<Documents> {
    ensure!(
        directory.names(2)? == ["project.json", "tasks"].map(std::ffi::OsString::from),
        "retained materialization candidate inventory changed"
    );
    let tasks = directory.child(Path::new("tasks"))?;
    let names = tasks.names(100_000)?;
    let mut documents = BTreeMap::new();
    let project = directory
        .read(Path::new("project.json"))?
        .context("retained project document missing")?;
    ensure!(
        project.len() <= 1024 * 1024,
        "retained project document exceeded bound"
    );
    let mut total = project.len();
    documents.insert("project.json".into(), project);
    for name in &names {
        let name = name.to_str().context("non-UTF8 retained task name")?;
        let id = uuid::Uuid::parse_str(
            name.strip_suffix(".json")
                .context("invalid retained task name")?,
        )?;
        ensure!(
            name == format!("{id}.json"),
            "noncanonical retained task name"
        );
        let bytes = tasks
            .read(Path::new(name))?
            .context("retained task document missing")?;
        ensure!(
            bytes.len() <= 1024 * 1024,
            "retained task document exceeded bound"
        );
        total = total
            .checked_add(bytes.len())
            .context("retained byte count overflow")?;
        ensure!(
            total <= 128 * 1024 * 1024,
            "retained candidate exceeded byte bound"
        );
        documents.insert(format!("tasks/{name}"), bytes);
    }
    ensure!(
        tasks.names(100_000)? == names,
        "retained task inventory changed during read"
    );
    TaskTree::from_stored_documents(&documents)?;
    Ok(documents)
}

pub(super) fn fingerprint(documents: &Documents) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    for (path, bytes) in documents {
        hash.update((path.len() as u64).to_le_bytes());
        hash.update(path.as_bytes());
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    format!("{:x}", hash.finalize())
}

pub(super) fn link_tree(
    source: &Directory,
    target: &Directory,
    documents: &Documents,
) -> Result<()> {
    let source_tasks = source.child(Path::new("tasks"))?;
    let target_tasks = target.create_child(Path::new("tasks"))?;
    source
        .dir()
        .hard_link("project.json", target.dir(), "project.json")?;
    for path in documents
        .keys()
        .filter_map(|path| path.strip_prefix("tasks/"))
    {
        source_tasks
            .dir()
            .hard_link(path, target_tasks.dir(), path)?;
    }
    Ok(())
}
